#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

fn pid() -> Pid {
    Pid {
        node: 1,
        id: 7,
        incarnation: 2,
    }
}

#[rstest]
#[case(ToApp::Init { args: b"{}".to_vec() })]
#[case(ToApp::Call { call_ref: 9, msg: Envelope::new("text/plain", b"hi".to_vec()) })]
#[case(ToApp::Info { info: Info::Down { reference: 3, pid: pid(), reason: ExitReason::Crashed("boom".into()) } })]
#[case(ToApp::Answer { id: 4, answer: Answer::Called(Err(CallError::Exited(ExitReason::Timeout))) })]
#[case(ToApp::Answer { id: 5, answer: Answer::CastFailed(CastError::Overloaded) })]
#[case(ToApp::Answer { id: 5, answer: Answer::CastFailed(CastError::Timeout) })]
#[case(ToApp::Ping { id: 5 })]
#[case(ToApp::LoadSnapshot { from_version: "1.0.0".into(), snapshot: vec![0, 255] })]
#[case(ToApp::SaveSnapshot)]
#[case(ToApp::Info { info: Info::KvChanged { watch: 2, bucket: "b".into(), key: "k".into(), entry: Some((Bytes(vec![0, 255]), 7)), revision: 7 } })]
#[case(ToApp::Answer { id: 6, answer: Answer::Kv(Ok(KvAnswer::Values(vec![Some(Bytes(vec![1])), None]))) })]
#[case(ToApp::Answer { id: 7, answer: Answer::Kv(Ok(KvAnswer::Page(ScanPage { entries: vec![("k".into(), Bytes(vec![2]), 3)], next: Some("l".into()), revision: 3 }))) })]
#[case(ToApp::Answer { id: 8, answer: Answer::Kv(Err(KvError::Conflict(Some(4)))) })]
#[case(ToApp::Answer { id: 9, answer: Answer::Groups(Ok(GroupsAnswer::Members(vec![pid()]))) })]
#[case(ToApp::Answer { id: 10, answer: Answer::Secret(Err(SecretError::Unavailable("gone".into()))) })]
#[case(ToApp::Answer { id: 11, answer: Answer::Sql(Ok(SqlAnswer::Rows(SqlRows { columns: vec!["id".into(), "b".into()], rows: vec![vec![SqlValue::Int(1), SqlValue::Bytes(Bytes(vec![0, 255]))], vec![SqlValue::Null, SqlValue::Float(0.5)]] }))) })]
#[case(ToApp::Answer { id: 12, answer: Answer::Sql(Err(SqlError::Query { code: "23505".into(), message: "dup".into() })) })]
#[case(ToApp::Answer { id: 13, answer: Answer::Net(Ok(NetAnswer::Stream(NetStream { socket: 2, fd: FdRef(5), local: "127.0.0.1:4000".parse().unwrap(), peer: "[::1]:80".parse().unwrap() }))) })]
#[case(ToApp::Answer { id: 14, answer: Answer::Net(Ok(NetAnswer::Datagram(Some(("10.0.0.1:53".parse().unwrap(), Bytes(vec![0, 255])))))) })]
#[case(ToApp::Answer { id: 15, answer: Answer::Net(Err(NetError::Limited)) })]
#[case(ToApp::Answer { id: 16, answer: Answer::Http(Ok(HttpResponse { status: 200, headers: vec![("content-type".into(), Bytes(b"text/plain".to_vec()))], body: Bytes(vec![0, 255]) })) })]
#[case(ToApp::Answer { id: 17, answer: Answer::Http(Err(HttpError::Tls("bad certificate".into()))) })]
#[case(ToApp::Answer { id: 18, answer: Answer::Extension(Ok(Bytes(vec![0x82, 0, 255]))) })]
#[case(ToApp::Answer { id: 19, answer: Answer::Extension(Err(ExtensionError::NoSuchFunction("greet#x".into()))) })]
fn encode____to_app_frames____read_back_unchanged(#[case] frame: ToApp) {
    let bytes = encode(&frame);

    let found: ToApp = read(&mut bytes.as_slice()).unwrap().unwrap();

    assert_eq!(found, frame);
}

#[rstest]
#[case(FromApp::Hello { version: VERSION })]
#[case(FromApp::Reply(None))]
#[case(FromApp::Snapshot(Bytes(vec![0, 255])))]
#[case(FromApp::Request { id: 1, request: Request::SendAfter { ms: 10, msg: Envelope::new("x", vec![0, 255]) } })]
#[case(FromApp::Request { id: 2, request: Request::Exit { to: pid(), reason: ExitReason::Killed } })]
#[case(FromApp::Log { level: Level::Warn, message: "careful".into() })]
#[case(FromApp::Request { id: 3, request: Request::Kv(KvRequest::SetMany { bucket: "b".into(), entries: vec![("k".into(), Bytes(vec![0, 255]))] }) })]
#[case(FromApp::Request { id: 5, request: Request::Groups(GroupsRequest::Publish { group: "g".into(), msg: Envelope::new("x", vec![1]) }) })]
#[case(FromApp::Request { id: 6, request: Request::MetricRegister { kind: MetricKind::Histogram, name: "lat".into(), help: "h".into(), labels: vec![("l".into(), "v".into())], buckets: vec![0.5, 1.0] } })]
#[case(FromApp::Metrics(vec![MetricUpdate::Add(1, 2.5), MetricUpdate::Observe(2, 0.25)]))]
#[case(FromApp::Request { id: 7, request: Request::Sql(SqlRequest::TxQuery { tx: 3, statement: "SELECT $1".into(), params: vec![SqlValue::Text("x".into()), SqlValue::Bool(true)] }) })]
#[case(FromApp::Request { id: 8, request: Request::Sql(SqlRequest::TxEnd { tx: 3, commit: false }) })]
#[case(FromApp::Request { id: 9, request: Request::Net(NetRequest::UdpSend { socket: 1, to: "127.0.0.1:9".parse().unwrap(), data: Bytes(vec![1]) }) })]
#[case(FromApp::Request { id: 10, request: Request::Net(NetRequest::ConnectHost { host: "example.com".into(), port: 443 }) })]
#[case(FromApp::Request { id: 11, request: Request::Http(HttpRequest { method: "POST".into(), url: "https://a.example/x".into(), headers: vec![("x".into(), Bytes(vec![1]))], body: Bytes(vec![0, 255]), timeout_ms: Some(500) }) })]
#[case(FromApp::Request { id: 12, request: Request::Extension { package: "example:greeting".into(), function: "greet#hello".into(), args: Bytes(vec![0x81, 0x61, b'a']) } })]
#[case(FromApp::Request { id: 4, request: Request::Kv(KvRequest::Scan { bucket: "b".into(), query: ScanQuery { prefix: Some("p".into()), limit: 10, ..ScanQuery::default() } }) })]
fn encode____from_app_frames____read_back_unchanged(#[case] frame: FromApp) {
    let bytes = encode(&frame);

    let found: FromApp = read(&mut bytes.as_slice()).unwrap().unwrap();

    assert_eq!(found, frame);
}

#[test]
fn encode____envelope_body____cbor_byte_string_not_array() {
    let body = vec![200u8; 100];

    let bytes = encode(&Envelope::new("x", body));

    // As an array, each byte over 23 would take two: over 200 in all.
    assert!(bytes.len() < 4 + 100 + 50, "{} bytes", bytes.len());
}

#[test]
fn encode____bytes_in_a_list____cbor_byte_strings_not_arrays() {
    let value = KvAnswer::Values(vec![Some(Bytes(vec![1, 2, 3]))]);

    let cbor = encode(&value);

    // A byte string of three: 0x43 then the bytes.
    assert!(cbor.windows(4).any(|w| w == [0x43, 1, 2, 3]), "{cbor:x?}");
}

#[test]
fn read____empty_stream____none() {
    let found: Option<ToApp> = read(&mut [].as_slice()).unwrap();

    assert!(found.is_none());
}

#[test]
fn read____length_over_max____too_large() {
    let prefix = u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes();

    let found = read::<ToApp>(&mut prefix.as_slice());

    assert!(matches!(found, Err(FrameError::TooLarge(_))));
}

#[test]
fn read____not_a_frame_of_the_type____decode_error() {
    let bytes = encode(&FromApp::Done);

    let found = read::<ToApp>(&mut bytes.as_slice());

    assert!(matches!(found, Err(FrameError::Decode(_))));
}

#[rstest]
#[case(VERSION, true)]
#[case(MIN_VERSION, true)]
#[case(MIN_VERSION - 1, false)]
#[case(VERSION + 1, false)]
fn accepts____a_version____only_the_current_and_the_one_before(
    #[case] version: u16,
    #[case] accepted: bool,
) {
    assert_eq!(accepts(version), accepted);
}

#[test]
fn since____every_frame____within_the_current_version() {
    let frames = [
        ToApp::SaveSnapshot,
        ToApp::Ping { id: 1 },
        ToApp::Info {
            info: Info::Timer {
                reference: 1,
                msg: Envelope::new("x", vec![]),
            },
        },
        ToApp::Connection {
            listener: "l".into(),
            stream: NetStream {
                socket: 1,
                fd: FdRef(1),
                local: "127.0.0.1:1".parse().unwrap(),
                peer: "127.0.0.1:2".parse().unwrap(),
            },
        },
    ];

    let since: Vec<u16> = frames.iter().map(ToApp::since).collect();

    assert_eq!(since, vec![1, 1, 1, 11]);
    assert!(since.iter().all(|v| *v <= VERSION));
}

#[test]
fn too_large____each_kind_of_request____its_own_error_or_none() {
    let msg = || Envelope::new("text/plain", "x");
    let n = MAX_FRAME + 1;

    let cast = too_large(
        &Request::Cast {
            to: pid(),
            msg: msg(),
        },
        n,
    );
    let call = too_large(
        &Request::Call {
            to: pid(),
            msg: msg(),
            timeout_ms: 1,
        },
        n,
    );
    let reply = too_large(
        &Request::Reply {
            call_ref: 1,
            msg: msg(),
        },
        n,
    );
    let kv = too_large(
        &Request::Kv(KvRequest::SetMany {
            bucket: "b".into(),
            entries: Vec::new(),
        }),
        n,
    );
    let timer = too_large(&Request::SendAfter { ms: 1, msg: msg() }, n);

    assert_eq!(cast, Some(Answer::CastFailed(CastError::TooLarge)));
    assert_eq!(call, Some(Answer::Called(Err(CallError::TooLarge))));
    assert_eq!(reply, Some(Answer::Unit));
    assert!(
        matches!(kv, Some(Answer::Kv(Err(KvError::Other(why)))) if why.contains("frame limit"))
    );
    assert_eq!(timer, None);
}

#[test]
fn answer_too_large____each_kind_of_answer____its_own_error_or_none() {
    let n = MAX_FRAME + 1;

    let kv = answer_too_large(&Answer::Kv(Ok(KvAnswer::Values(Vec::new()))), n);
    let called = answer_too_large(&Answer::Called(Ok(Envelope::new("x", "y"))), n);
    let sql = answer_too_large(&Answer::Sql(Err(SqlError::Timeout)), n);
    let unit = answer_too_large(&Answer::Unit, n);
    let extension = answer_too_large(&Answer::Extension(Ok(Bytes(Vec::new()))), n);

    assert!(
        matches!(kv, Some(Answer::Kv(Err(KvError::Other(why)))) if why.contains("frame limit"))
    );
    assert_eq!(called, Some(Answer::Called(Err(CallError::TooLarge))));
    assert_eq!(sql, Some(Answer::Sql(Err(SqlError::TooLarge))));
    assert_eq!(unit, None);
    assert_eq!(extension, None);
}

#[test]
fn over_limit____encoded_frame____only_a_body_past_max_frame() {
    let at = vec![0u8; 4 + MAX_FRAME];
    let past = vec![0u8; 5 + MAX_FRAME];

    assert!(!over_limit(&at));
    assert!(over_limit(&past));
}

#[test]
fn trace____traceparent____formats_and_parses_back_sampled() {
    let trace = Trace::root_from(std::array::from_fn(|i| u8::try_from(i).unwrap() + 1));

    let header = trace.traceparent();
    let back = Trace::from_traceparent(&header);

    assert_eq!(
        header,
        "00-0102030405060708090a0b0c0d0e0f10-1112131415161718-01"
    );
    assert_eq!(back, Some((trace, true)));
    assert_eq!(trace.trace_hex(), "0102030405060708090a0b0c0d0e0f10");
}

#[rstest]
#[case("00-0102030405060708090a0b0c0d0e0f10-1112131415161718-00", Some(false))]
#[case(
    "01-0102030405060708090a0b0c0d0e0f10-1112131415161718-01-later",
    Some(true)
)]
#[case("00-0102030405060708090a0b0c0d0e0f10-1112131415161718-01-extra", None)]
#[case("ff-0102030405060708090a0b0c0d0e0f10-1112131415161718-01", None)]
#[case("00-00000000000000000000000000000000-1112131415161718-01", None)]
#[case("00-0102030405060708090a0b0c0d0e0f10-0000000000000000-01", None)]
#[case("00-0102030405060708090A0B0C0D0E0F10-1112131415161718-01", None)]
#[case("00-0102-1112131415161718-01", None)]
#[case("", None)]
fn trace____from_traceparent____valid_ones_only(
    #[case] header: &str,
    #[case] sampled: Option<bool>,
) {
    assert_eq!(Trace::from_traceparent(header).map(|(_, s)| s), sampled);
}

#[test]
fn trace____child____the_same_trace_a_new_span() {
    let root = Trace::root();

    let child = root.child();

    assert_eq!(child.trace_id, root.trace_id);
    assert_ne!(child.span_id, root.span_id);
    assert_ne!(Trace::root().trace_id, root.trace_id);
}

#[test]
fn envelope____traced____round_trips_untraced_encodes_as_before() {
    let traced = Envelope {
        trace: Some(Box::new(Trace::root_from([9; 24]))),
        ..Envelope::new("text/plain", "x")
    };
    let plain = Envelope::new("text/plain", "x");

    let back: Envelope = decode(&encode(&traced)[4..]).unwrap();
    let plain_bytes = encode(&plain);

    assert_eq!(back, traced);
    assert!(
        !plain_bytes.windows(5).any(|w| w == b"trace"),
        "no trace key when untraced"
    );
}

#[test]
fn trace____every_byte_value____lowercase_hex_two_digits_each() {
    let mut random = [0u8; 24];
    for (i, b) in random.iter_mut().enumerate() {
        *b = u8::try_from(i).unwrap() * 11;
    }
    let trace = Trace::root_from(random);
    let want: String = random[..16].iter().map(|b| format!("{b:02x}")).collect();

    assert_eq!(trace.trace_hex(), want);
    for b in 0..=255u8 {
        let t = Trace::root_from([b; 24]);
        assert_eq!(t.trace_hex(), format!("{b:02x}").repeat(16));
        assert_eq!(
            Trace::from_traceparent(&t.traceparent())
                .map(|(t, _)| t)
                .filter(|_| b != 0),
            (b != 0).then_some(t)
        );
    }
}

fn bulk(n: usize) -> Envelope {
    Envelope::new("application/octet-stream", vec![9; n])
}

#[rstest]
#[case::call(ToApp::Call { call_ref: 3, msg: bulk(70_000) }, 70_000)]
#[case::cast(ToApp::Cast { msg: bulk(5) }, 5)]
#[case::called(ToApp::Answer { id: 1, answer: Answer::Called(Ok(bulk(100_000))) }, 100_000)]
#[case::ping(ToApp::Ping { id: 1 }, 0)]
fn to_app_size_hint____frames____their_largest_body(#[case] frame: ToApp, #[case] hint: usize) {
    let got = frame.size_hint();

    assert_eq!(got, hint);
}

#[rstest]
#[case::reply(FromApp::Reply(Some(bulk(70_000))), 70_000)]
#[case::call_app(FromApp::Request { id: 1, request: Request::CallApp { app: "a".into(), msg: bulk(200_000), timeout_ms: 1, partner: false } }, 200_000)]
#[case::done(FromApp::Done, 0)]
fn from_app_size_hint____frames____their_largest_body(#[case] frame: FromApp, #[case] hint: usize) {
    let got = frame.size_hint();

    assert_eq!(got, hint);
}

#[rstest]
#[case::no_hint(0)]
#[case::the_body(1 << 20)]
#[case::too_small(10)]
fn encode_sized____any_hint____the_bytes_encode_gives(#[case] hint: usize) {
    let frame = ToApp::Call {
        call_ref: 3,
        msg: bulk(1 << 20),
    };

    let sized = encode_sized(&frame, hint);

    assert_eq!(sized, encode(&frame));
    assert_eq!(decode::<ToApp>(&sized[4..]).unwrap(), frame);
}
