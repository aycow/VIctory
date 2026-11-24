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

mod gpu;
