#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use bulkhead_native::protocol::Pid;

use super::*;

fn pid() -> Pid {
    Pid {
        node: 1,
        id: 2,
        incarnation: 1,
    }
}

#[test]
fn stamp_request____a_call_in_a_traced_callback____a_child_span_of_its_trace() {
    let current = Trace::root();
    let mut call = Request::Call {
        to: pid(),
        msg: Envelope::new("t", "x"),
        timeout_ms: 1,
    };

    stamp_request(&mut call, Some(current));

    let Request::Call { msg, .. } = call else {
        unreachable!()
    };
    let trace = msg.trace.unwrap();
    assert_eq!(trace.trace_id, current.trace_id);
    assert_ne!(trace.span_id, current.span_id);
}

#[test]
fn stamp_request____untraced_callback_or_a_trace_of_the_apps____unchanged() {
    let own = Trace::root();
    let mut untraced = Request::Cast {
        to: pid(),
        msg: Envelope::new("t", "x"),
    };
    let mut set = Request::Cast {
        to: pid(),
        msg: Envelope {
            trace: Some(Box::new(own)),
            ..Envelope::new("t", "x")
        },
    };

    stamp_request(&mut untraced, None);
    stamp_request(&mut set, Some(Trace::root()));

    let (Request::Cast { msg: a, .. }, Request::Cast { msg: b, .. }) = (untraced, set) else {
        unreachable!()
    };
    assert_eq!(a.trace, None);
    assert_eq!(b.trace.as_deref(), Some(&own));
}

#[test]
fn stamp_request____a_group_publish____stamped_a_reply_not() {
    let current = Some(Trace::root());
    let mut publish = Request::Groups(GroupsRequest::Publish {
        group: "g".into(),
        msg: Envelope::new("t", "x"),
    });
    let mut reply = Request::Reply {
        call_ref: 1,
        msg: Envelope::new("t", "x"),
    };

    stamp_request(&mut publish, current);
    stamp_request(&mut reply, current);

    let Request::Groups(GroupsRequest::Publish { msg, .. }) = publish else {
        unreachable!()
    };
    let Request::Reply { msg: r, .. } = reply else {
        unreachable!()
    };
    assert!(msg.trace.is_some());
    assert_eq!(r.trace, None);
}
