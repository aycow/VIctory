//! Opt-in provenance of submitted texture work, keyed by present count.
//!
//! Edges describe binding metadata and command order. A draw, UAV dispatch
//! or partial copy is a possible writer, not proof that any pixel changed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use crate::object::This;

const LIST_LIMIT: usize = 4096;
const FRAME_LIMIT: usize = 16_384;
const RESOURCE_LIMIT: usize = 32_768;
const ACCESS_LIMIT: usize = 256;

static FINISHED: AtomicBool = AtomicBool::new(false);

fn window() -> Option<(u64, u64)> {
    static WINDOW: OnceLock<Option<(u64, u64)>> = OnceLock::new();
    *WINDOW.get_or_init(|| {
        let value = std::env::var("VICTORY_RENDER_DEPENDENCIES").ok()?;
        let (first, last) = value.split_once('-').unwrap_or((&value, &value));
        match (first.parse::<u64>(), last.parse::<u64>()) {
            (Ok(first), Ok(last)) if first > 0 && first <= last => Some((first, last)),
            _ => {
                tracing::warn!("VICTORY_RENDER_DEPENDENCIES needs a present number or inclusive first-last range");
                None
            }
        }
    })
}

pub(crate) fn enabled() -> bool {
    !FINISHED.load(Relaxed) && window().is_some()
}

pub(crate) type Resource = u64;

pub(crate) fn new_resource() -> Resource {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    if enabled() {
        NEXT.fetch_add(1, Relaxed)
    } else {
        0
    }
}

pub(crate) fn texture(resource: This) -> Option<Resource> {
    let id = crate::resource::host_copy(resource)?.dependency_id;
    (id != 0).then_some(id)
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Role {
    Target,
    Depth,
    Srv,
    Uav,
    CopySource,
    CopyDest,
    Clear,
    Upload,
    Present,
}

impl Role {
    fn input(self) -> bool {
        matches!(
            self,
            Self::Srv | Self::Uav | Self::Target | Self::Depth | Self::CopySource | Self::Present
        )
    }

    fn expects_writer(self) -> bool {
        matches!(self, Self::Srv | Self::CopySource | Self::Present)
    }

    fn output(self) -> bool {
        matches!(
            self,
            Self::Target | Self::Depth | Self::Uav | Self::CopyDest | Self::Clear | Self::Upload
        )
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Access {
    pub(crate) resource: Option<Resource>,
    pub(crate) role: Role,
    pub(crate) extent: Option<(u64, u32, i32)>,
    pub(crate) binding: Option<Binding>,
}

impl Access {
    pub(crate) fn texture(resource: This, role: Role) -> Self {
        let resource_id = texture(resource);
        let extent = crate::resource::host_resource(resource).map(|host| {
            // SAFETY: the host copy is live.
            let desc = unsafe { host.GetDesc() };
            (desc.Width, desc.Height, desc.Format.0)
        });
        Self {
            resource: resource_id,
            role,
            extent,
            binding: None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Binding {
    pub(crate) stage: &'static str,
    pub(crate) root: u8,
    pub(crate) offset_dwords: u32,
    pub(crate) host_slot: Option<u32>,
    pub(crate) title_resource: Option<Resource>,
    pub(crate) title_view: &'static str,
    pub(crate) host_resource_matches: Option<bool>,
}

#[derive(Clone)]
pub(crate) enum Event {
    Pass {
        kind: &'static str,
        key: String,
        shaders: String,
        refusal: Option<String>,
        accesses: Vec<Access>,
    },
    State {
        resource: Resource,
        before: u32,
        after: u32,
        prelude: bool,
    },
    Present {
        frame: u64,
        source: Access,
        state_before_copy: Option<u32>,
    },
}

impl Event {
    pub(crate) fn transfer(kind: &'static str, accesses: Vec<Access>) -> Self {
        Self::Pass {
            kind,
            key: String::new(),
            shaders: String::new(),
            refusal: None,
            accesses,
        }
    }
}

#[derive(Default)]
pub(crate) struct Recorded {
    events: Vec<Event>,
    omitted: usize,
}

impl Recorded {
    pub(crate) fn push(&mut self, mut event: Event) {
        if self.events.len() < LIST_LIMIT {
            if let Event::Pass { accesses, .. } = &mut event
                && accesses.len() > ACCESS_LIMIT
            {
                self.omitted += accesses.len() - ACCESS_LIMIT;
                accesses.truncate(ACCESS_LIMIT);
            }
            self.events.push(event);
        } else {
            self.omitted += 1;
        }
    }

    fn replay(&self, tracker: &mut Tracker) -> Vec<Report> {
        tracker.omitted += self.omitted;
        self.events
            .iter()
            .filter_map(|event| tracker.observe(event))
            .collect()
    }
}

#[derive(Clone, Copy, Debug)]
struct Point {
    frame: u64,
    sequence: u64,
}

#[derive(Default)]
struct Last {
    writer: Option<Point>,
    refused_writer: Option<Point>,
    state: Option<u32>,
}

struct Input {
    access: Access,
    writer: Option<Point>,
    refused_writer: Option<Point>,
    state: Option<u32>,
}

struct Node {
    point: Point,
    event: Event,
    inputs: Vec<Input>,
}

#[derive(Default)]
pub(crate) struct Tracker {
    next_frame: u64,
    sequence: u64,
    resources: BTreeMap<Resource, Last>,
    events: Vec<Node>,
    omitted: usize,
}

impl Tracker {
    fn track(&mut self, id: Resource) -> Option<&mut Last> {
        if self.resources.len() == RESOURCE_LIMIT && !self.resources.contains_key(&id) {
            self.omitted += 1;
            return None;
        }
        Some(self.resources.entry(id).or_default())
    }

    fn observe(&mut self, event: &Event) -> Option<Report> {
        let frame = self.next_frame.max(1);
        self.sequence += 1;
        let point = Point {
            frame,
            sequence: self.sequence,
        };
        let mut inputs = Vec::new();
        let present = match event {
            Event::Pass {
                refusal, accesses, ..
            } => {
                if refusal.is_none() {
                    for &access in accesses.iter().filter(|a| a.role.input()) {
                        inputs.push(self.input(access));
                    }
                }
                for id in accesses
                    .iter()
                    .filter(|a| a.role.output())
                    .filter_map(|a| a.resource)
                {
                    if let Some(last) = self.track(id) {
                        if refusal.is_some() {
                            last.refused_writer = Some(point);
                        } else {
                            last.writer = Some(point);
                            last.refused_writer = None;
                        }
                    }
                }
                None
            }
            Event::State {
                resource, after, ..
            } => {
                if let Some(last) = self.track(*resource) {
                    last.state = Some(*after);
                }
                None
            }
            Event::Present {
                frame,
                source,
                state_before_copy,
            } => {
                let mut input = self.input(*source);
                input.state = (*state_before_copy).or(input.state);
                inputs.push(input);
                Some(*frame)
            }
        };
        if self.events.len() < FRAME_LIMIT || present.is_some() {
            self.events.push(Node {
                point,
                event: event.clone(),
                inputs,
            });
        } else {
            self.omitted += 1;
        }
        present.map(|frame| {
            self.next_frame = frame.saturating_add(1);
            Report {
                frame,
                nodes: std::mem::take(&mut self.events),
                omitted: std::mem::take(&mut self.omitted),
            }
        })
    }

    fn input(&self, access: Access) -> Input {
        let last = access.resource.and_then(|id| self.resources.get(&id));
        Input {
            access,
            writer: last.and_then(|last| last.writer),
            refused_writer: last.and_then(|last| last.refused_writer),
            state: last.and_then(|last| last.state),
        }
    }
}

pub(crate) struct Report {
    frame: u64,
    nodes: Vec<Node>,
    omitted: usize,
}

impl Report {
    pub(crate) fn publish(self) {
        self.log();
        #[cfg(test)]
        tests::capture_report(self);
    }

    fn lineage(&self) -> BTreeSet<u64> {
        let mut lineage = BTreeSet::new();
        if let Some(node) = self.nodes.last() {
            lineage.insert(node.point.sequence);
        }
        // Every writer precedes its consumer. Track refused writes as a
        // separate candidate edge, so they cannot hide an earlier writer.
        for node in self.nodes.iter().rev() {
            if lineage.contains(&node.point.sequence) {
                for input in &node.inputs {
                    lineage.extend(
                        [input.writer, input.refused_writer]
                            .into_iter()
                            .flatten()
                            .map(|p| p.sequence),
                    );
                }
            }
        }
        lineage
    }

    fn log(&self) {
        let lineage = self.lineage();
        let no_prior_writer = self
            .nodes
            .iter()
            .flat_map(|n| &n.inputs)
            .filter(|i| {
                i.access.role.expects_writer() && i.access.resource.is_some() && i.writer.is_none()
            })
            .count();
        let unresolved_shader_bindings = self
            .nodes
            .iter()
            .flat_map(|n| &n.inputs)
            .filter(|i| i.access.binding.is_some() && i.access.resource.is_none())
            .count();
        let refused = self
            .nodes
            .iter()
            .filter(|n| {
                matches!(
                    &n.event,
                    Event::Pass {
                        refusal: Some(_),
                        ..
                    }
                )
            })
            .count();
        tracing::info!(
            target: "victory_gpu::dependencies", frame = self.frame, operations = self.nodes.len(),
            no_prior_writer, unresolved_shader_bindings, refused, omitted = self.omitted, "render dependencies"
        );
        for node in &self.nodes {
            let sequence = node.point.sequence;
            let on_path = lineage.contains(&sequence);
            match &node.event {
                Event::State {
                    resource,
                    before,
                    after,
                    prelude,
                } => tracing::info!(
                    target: "victory_gpu::dependencies", frame = self.frame, sequence, resource,
                    before, after, prelude, "render resource state"
                ),
                Event::Pass {
                    kind,
                    key,
                    shaders,
                    refusal,
                    accesses,
                } => {
                    if accesses.is_empty() && refusal.is_none() {
                        continue;
                    }
                    tracing::info!(
                        target: "victory_gpu::dependencies", frame = self.frame, sequence, on_path,
                        kind, key, shaders, refusal = refusal.as_deref(), "render operation"
                    );
                    for access in accesses.iter().filter(|a| a.role.output()) {
                        self.log_access(sequence, "output", access, None);
                    }
                }
                Event::Present { source, .. } => tracing::info!(
                    target: "victory_gpu::dependencies", frame = self.frame, sequence,
                    resource = source.resource, extent = ?source.extent, "render presentation source"
                ),
            }
            for input in &node.inputs {
                self.log_access(sequence, "input", &input.access, Some(input));
            }
        }
    }

    fn log_access(&self, sequence: u64, direction: &str, access: &Access, input: Option<&Input>) {
        let writer = input.and_then(|i| i.writer);
        let refused = input.and_then(|i| i.refused_writer);
        let state = input.and_then(|i| i.state);
        let previous_frame = writer.is_some_and(|w| w.frame < self.frame);
        tracing::info!(
            target: "victory_gpu::dependencies", frame = self.frame, sequence, direction,
            resource = access.resource, extent = ?access.extent, role = ?access.role, state, previous_frame,
            writer = writer.map(|w| w.sequence), writer_frame = writer.map(|w| w.frame),
            refused = refused.map(|w| w.sequence), refused_frame = refused.map(|w| w.frame),
            stage = access.binding.map(|b| b.stage), root = access.binding.map(|b| b.root),
            offset_dwords = access.binding.map(|b| b.offset_dwords),
            host_slot = access.binding.and_then(|b| b.host_slot),
            title_view = access.binding.map(|b| b.title_view),
            title_resource = access.binding.and_then(|b| b.title_resource),
            host_resource_matches = access.binding.and_then(|b| b.host_resource_matches), "render resource access"
        );
    }
}

static TRACKER: Mutex<Option<Tracker>> = Mutex::new(None);

pub(crate) fn submitted(prelude: &Recorded, recorded: &Recorded) -> Vec<Report> {
    if !enabled() {
        return Vec::new();
    }
    let reports = {
        let mut guard = TRACKER.lock().unwrap_or_else(|e| e.into_inner());
        let tracker = guard.get_or_insert_with(Tracker::default);
        prelude
            .replay(tracker)
            .into_iter()
            .chain(recorded.replay(tracker))
            .collect::<Vec<_>>()
    };
    let (first, last) = window().expect("enabled render dependencies");
    if reports.iter().any(|report| report.frame >= last) {
        FINISHED.store(true, Relaxed);
        *TRACKER.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    reports
        .into_iter()
        .filter(|report| (first..=last).contains(&report.frame))
        .collect()
}

pub(crate) fn presentation_result(
    frame: u64,
    source: Option<Resource>,
    black: Option<bool>,
    shown: bool,
) {
    if window().is_some_and(|(first, last)| (first..=last).contains(&frame)) {
        tracing::info!(target: "victory_gpu::dependencies", frame, source, black, shown, "render presentation result");
    }
}

#[cfg(test)]
mod tests;
