use super::*;

thread_local! {
    static REPORTS: std::cell::RefCell<Option<Vec<Report>>> = const { std::cell::RefCell::new(None) };
}

fn capture(run: impl FnOnce()) -> Vec<Report> {
    REPORTS.set(Some(Vec::new()));
    run();
    REPORTS.take().unwrap()
}

pub(super) fn capture_report(report: Report) {
    REPORTS.with_borrow_mut(|reports| {
        if let Some(reports) = reports {
            reports.push(report);
        }
    });
}

fn access(resource: Resource, role: Role) -> Access {
    Access {
        resource: Some(resource),
        role,
        extent: Some((1, 1, 28)),
        binding: None,
    }
}

fn operation(kind: &'static str, refusal: Option<&str>, accesses: Vec<Access>) -> Event {
    let mut event = Event::transfer(kind, accesses);
    let Event::Pass { refusal: out, .. } = &mut event else {
        unreachable!()
    };
    *out = refusal.map(str::to_owned);
    event
}

fn refuse(pipeline: &str, shaders: &str, reason: &str, accesses: Vec<Access>) -> Event {
    Event::Pass {
        kind: "dispatch",
        key: pipeline.into(),
        shaders: shaders.into(),
        refusal: Some(reason.into()),
        accesses,
    }
}

fn present(frame: u64, source: Resource) -> Event {
    Event::Present {
        frame,
        source: access(source, Role::Present),
        state_before_copy: None,
    }
}

#[test]
fn lineage_distinguishes_absent_previous_and_refused_writers() {
    let mut tracker = Tracker::default();
    let mut record = Recorded::default();
    record.push(Event::transfer("seed", vec![access(1, Role::Clear)]));
    record.push(present(1, 1));
    let report = record.replay(&mut tracker).pop().unwrap();
    assert_eq!(report.lineage().into_iter().collect::<Vec<_>>(), [1, 2]);

    let mut record = Recorded::default();
    record.push(operation(
        "draw",
        Some("translation refused"),
        vec![access(1, Role::Target)],
    ));
    record.push(Event::State {
        resource: 1,
        before: 4,
        after: 64,
        prelude: true,
    });
    record.push(Event::transfer(
        "dispatch",
        vec![
            access(1, Role::Srv),
            access(2, Role::Srv),
            access(3, Role::Uav),
        ],
    ));
    record.push(Event::transfer(
        "copy",
        vec![access(3, Role::CopySource), access(4, Role::CopyDest)],
    ));
    record.push(present(2, 4));
    let report = record.replay(&mut tracker).pop().unwrap();
    let inputs = &report.nodes[2].inputs;
    assert_eq!(inputs[0].writer.unwrap().sequence, 1);
    assert_eq!(inputs[0].writer.unwrap().frame, 1);
    assert_eq!(inputs[0].refused_writer.unwrap().sequence, 3);
    assert_eq!(inputs[0].state, Some(64));
    assert!(inputs[1].writer.is_none());
    assert!(inputs[1].refused_writer.is_none());
    assert_eq!(
        report.lineage().into_iter().collect::<Vec<_>>(),
        [1, 3, 5, 6, 7]
    );
}

#[test]
fn refused_producers_count_only_reachable_operations_once() {
    let mut record = Recorded::default();
    let output = |id| access(id, Role::Uav);
    let refusals: [(_, _, _, &[_]); 8] = [
        ("lighting", "cs:a", "translation", &[12]),
        ("lighting", "cs:a", "translation", &[10, 11]),
        ("unrelated", "cs:z", "translation", &[13]),
        ("lighting", "cs:a", "translation", &[14]),
        ("lighting", "cs:a", "descriptor", &[15]),
        ("lighting", "cs:b", "translation", &[16]),
        ("lighting-other", "cs:a", "translation", &[17]),
        ("lighting", "cs:a", "translation", &[18]),
    ];
    for (pipeline, shaders, reason, outputs) in refusals {
        record.push(refuse(
            pipeline,
            shaders,
            reason,
            outputs.iter().copied().map(output).collect(),
        ));
    }
    record.push(Event::transfer("replaced", vec![access(18, Role::Clear)]));
    record.push(Event::transfer(
        "compose",
        [10, 11, 14, 15, 16, 17, 18]
            .into_iter()
            .map(|id| access(id, Role::Srv))
            .chain([output(19)])
            .collect(),
    ));
    record.push(present(1, 19));

    let report = record.replay(&mut Tracker::default()).pop().unwrap();
    let groups = report.refused_producers(&report.lineage());
    let summary = |pipeline, shaders, reason, count, earliest_sequence| {
        (
            RefusedProducer {
                pipeline,
                shaders,
                reason,
            },
            RefusalCount {
                count,
                earliest_sequence,
            },
        )
    };
    assert_eq!(
        groups,
        BTreeMap::from([
            summary("lighting", "cs:a", "translation", 2, 2),
            summary("lighting", "cs:a", "descriptor", 1, 5),
            summary("lighting", "cs:b", "translation", 1, 6),
            summary("lighting-other", "cs:a", "translation", 1, 7),
        ]),
    );
}

mod gpu;
