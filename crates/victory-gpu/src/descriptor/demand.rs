//! Opt-in aggregate demand for tables rebuilt on capped view heaps.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use victory_shader::{ResourceType, UsedResource};

use super::{TableReject, host::HostHeap};

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| crate::switch::on("VICTORY_DESCRIPTOR_DEMAND"))
}

/// Run the sharing-off control with the same diagnostics and binary.
pub(crate) fn sharing() -> bool {
    static SHARING: OnceLock<bool> = OnceLock::new();
    *SHARING.get_or_init(|| !crate::switch::on("VICTORY_DESCRIPTOR_DEMAND_NO_SHARE"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    CbvOnly,
    MixedCbv,
    OtherShaderAccess,
    IneligibleSrv(TableReject),
    FixedTextureSrv,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Outcome {
    SharedNew,
    SharedSameList,
    SharedOtherList,
    LocalNew,
    LocalReuse,
    NoRange,
}

pub(crate) struct Request {
    width: u32,
    class: Class,
    pub(crate) share_unavailable: bool,
}

impl Request {
    pub(crate) fn new(width: u32, entries: &[&UsedResource]) -> Option<Self> {
        enabled().then(|| {
            let class = if entries.iter().all(|u| u.kind == ResourceType::Cbv) {
                Class::CbvOnly
            } else if entries.iter().any(|u| u.kind == ResourceType::Cbv) {
                Class::MixedCbv
            } else {
                Class::OtherShaderAccess
            };
            Self { width, class, share_unavailable: false }
        })
    }

    pub(crate) fn eligible(&mut self) {
        self.class = Class::FixedTextureSrv;
    }

    pub(crate) fn rejected(&mut self, reason: TableReject) {
        self.class = Class::IneligibleSrv(reason);
    }

    pub(crate) fn finish(self, outcome: Outcome, heap: &HostHeap) {
        let mut report = report();
        report.add(self, outcome, heap.demand_pressure());
        static FIRST_REFUSAL: AtomicBool = AtomicBool::new(true);
        if outcome == Outcome::NoRange && FIRST_REFUSAL.swap(false, Relaxed) {
            emit(&mut report, crate::queue::presented() + 1, "first_refusal");
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Counts {
    requests: u64,
    slots: u64,
    refused: u64,
    refused_slots: u64,
}

impl Counts {
    fn add(&mut self, width: u32, refused: bool) {
        self.requests += 1;
        self.slots += u64::from(width);
        if refused {
            self.refused += 1;
            self.refused_slots += u64::from(width);
        }
    }
}

#[derive(Default)]
struct Report {
    through: u64,
    total: Counts,
    widths: BTreeMap<&'static str, Counts>,
    classes: BTreeMap<Class, Counts>,
    outcomes: BTreeMap<Outcome, Counts>,
    share_unavailable: Counts,
    /// Capacity -> (largest live sample, largest process peak), in slots.
    pressure: BTreeMap<u32, (usize, usize)>,
}

impl Report {
    fn add(&mut self, request: Request, outcome: Outcome, (capacity, live, peak): (u32, usize, usize)) {
        let width = request.width;
        let refused = outcome == Outcome::NoRange;
        self.total.add(width, refused);
        self.widths.entry(width_bucket(width)).or_default().add(width, refused);
        self.classes.entry(request.class).or_default().add(width, refused);
        self.outcomes.entry(outcome).or_default().add(width, refused);
        if request.share_unavailable {
            self.share_unavailable.add(width, refused);
        }
        let pressure = self.pressure.entry(capacity).or_default();
        pressure.0 = pressure.0.max(live);
        pressure.1 = pressure.1.max(peak);
    }
}

fn width_bucket(width: u32) -> &'static str {
    match width {
        1 => "1",
        2..=4 => "2-4",
        5..=16 => "5-16",
        17..=64 => "17-64",
        65..=256 => "65-256",
        257..=1024 => "257-1024",
        1025..=4096 => "1025-4096",
        _ => "4097+",
    }
}

fn report() -> std::sync::MutexGuard<'static, Report> {
    static REPORT: OnceLock<Mutex<Report>> = OnceLock::new();
    REPORT.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn presented(frame: u64) {
    if enabled() && (frame.is_power_of_two() || frame.is_multiple_of(64)) {
        let mut report = report();
        emit(&mut report, frame, "present_window");
        report.through = frame;
    }
}

fn emit(report: &mut Report, frame: u64, event: &'static str) {
    let previous_present = report.through;
    let sample = std::mem::take(report);
    report.through = previous_present;
    tracing::info!(
        event,
        sharing = sharing(),
        previous_present,
        through_present = frame,
        total = ?sample.total,
        widths = ?sample.widths,
        classes = ?sample.classes,
        outcomes = ?sample.outcomes,
        share_unavailable = ?sample.share_unavailable,
        pressure = ?sample.pressure,
        "rebuilt descriptor demand"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_actual_eligibility_reuse_failures_and_pressure() {
        let mut report = Report::default();
        let mut add = |width, class, outcome, pressure| {
            report.add(Request { width, class, share_unavailable: false }, outcome, pressure);
        };
        add(2048, Class::FixedTextureSrv, Outcome::SharedNew, (65536, 2048, 2048));
        add(2048, Class::FixedTextureSrv, Outcome::SharedOtherList, (65536, 2048, 2048));
        add(2048, Class::FixedTextureSrv, Outcome::SharedSameList, (65536, 2048, 2048));
        add(1, Class::MixedCbv, Outcome::LocalNew, (65536, 2049, 2049));
        add(2048, Class::IneligibleSrv(TableReject::ReplaceableBacking), Outcome::NoRange, (65536, 65000, 65536));
        assert_eq!(report.total, Counts { requests: 5, slots: 8193, refused: 1, refused_slots: 2048 });
        assert_eq!(report.classes[&Class::FixedTextureSrv].slots, 6144);
        assert_eq!(report.classes[&Class::MixedCbv].slots, 1);
        assert_eq!(report.outcomes[&Outcome::SharedOtherList].slots, 2048);
        assert_eq!(report.outcomes[&Outcome::SharedSameList].slots, 2048);
        assert_eq!(report.widths["1025-4096"].refused, 1);
        assert_eq!(report.pressure[&65536], (65000, 65536));
    }
}
