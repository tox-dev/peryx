use crate::{
    CHANGELOG_PAGE_SIZE, ChangelogEntry, ChangelogPage, ChangelogRequest, ChangelogRequestError,
    dispatch_changelog_request, parse_changelog_request, render_changelog_fault, render_changelog_response,
    render_last_serial_response,
};

#[test]
fn test_parse_changelog_last_serial_request() {
    assert_eq!(
        parse_changelog_request(
            br#"<?xml version="1.0"?><methodCall><methodName>changelog_last_serial</methodName><params/></methodCall>"#,
        ),
        Ok(ChangelogRequest::LastSerial)
    );
}

#[test]
fn test_parse_changelog_request_accepts_whitespace_and_cdata() {
    assert_eq!(
        parse_changelog_request(
            b"<methodCall>\n<methodName><![CDATA[ changelog_since_serial ]]></methodName>\n<params><param><value><i8> 42 </i8></value></param></params>\n</methodCall>",
        ),
        Ok(ChangelogRequest::SinceSerial(42))
    );
}

#[rstest::rstest]
#[case::int("int")]
#[case::i4("i4")]
#[case::i8("i8")]
fn test_parse_changelog_since_serial_request(#[case] integer_tag: &str) {
    let request = format!(
        "<?xml version=\"1.0\"?><methodCall><methodName>changelog_since_serial</methodName><params><param><value><{integer_tag}>-1</{integer_tag}></value></param></params></methodCall>"
    );
    assert_eq!(
        parse_changelog_request(request.as_bytes()),
        Ok(ChangelogRequest::SinceSerial(-1))
    );
}

#[rstest::rstest]
#[case::invalid_utf8(&[0xff], ChangelogRequestError::InvalidUtf8)]
#[case::malformed_xml(b"<methodCall>", ChangelogRequestError::MalformedXml)]
#[case::missing_method(
    b"<methodCall/>",
    ChangelogRequestError::InvalidShape("expected one methodCall and methodName")
)]
#[case::unknown_method(
    b"<methodCall><methodName>list_packages</methodName></methodCall>",
    ChangelogRequestError::UnsupportedMethod("list_packages".to_owned())
)]
#[case::extra_parameter(
    b"<methodCall><methodName>changelog_last_serial</methodName><params><param/></params></methodCall>",
    ChangelogRequestError::InvalidShape("changelog_last_serial takes no parameters")
)]
#[case::missing_serial(
    b"<methodCall><methodName>changelog_since_serial</methodName><params><param/></params></methodCall>",
    ChangelogRequestError::InvalidShape("changelog_since_serial takes one integer")
)]
#[case::missing_parameter(
    b"<methodCall><methodName>changelog_since_serial</methodName><params/></methodCall>",
    ChangelogRequestError::InvalidShape("changelog_since_serial takes one integer")
)]
#[case::mixed_value_children(
    b"<methodCall><methodName>changelog_since_serial</methodName><params><param><value><int>1</int><string>x</string></value></param></params></methodCall>",
    ChangelogRequestError::InvalidShape("changelog_since_serial takes one integer")
)]
#[case::duplicate_integer_children(
    b"<methodCall><methodName>changelog_since_serial</methodName><params><param><value><int>1</int><i8>2</i8></value></param></params></methodCall>",
    ChangelogRequestError::InvalidShape("changelog_since_serial takes one integer")
)]
#[case::duplicate_values(
    b"<methodCall><methodName>changelog_since_serial</methodName><params><param><value><int>1</int></value><value><int>2</int></value></param></params></methodCall>",
    ChangelogRequestError::InvalidShape("changelog_since_serial takes one integer")
)]
#[case::invalid_serial(
    b"<methodCall><methodName>changelog_since_serial</methodName><params><param><value><int>x</int></value></param></params></methodCall>",
    ChangelogRequestError::InvalidSerial("x".to_owned())
)]
fn test_parse_changelog_request_rejects(#[case] body: &[u8], #[case] expected: ChangelogRequestError) {
    assert_eq!(parse_changelog_request(body), Err(expected));
}

#[rstest::rstest]
#[case::invalid_utf8(ChangelogRequestError::InvalidUtf8, "XML-RPC request is not UTF-8")]
#[case::malformed_xml(ChangelogRequestError::MalformedXml, "XML-RPC request is malformed")]
#[case::invalid_shape(
    ChangelogRequestError::InvalidShape("missing method"),
    "invalid XML-RPC request: missing method"
)]
#[case::unsupported_method(
    ChangelogRequestError::UnsupportedMethod("list_packages".to_owned()),
    "unsupported XML-RPC method \"list_packages\""
)]
#[case::invalid_serial(
    ChangelogRequestError::InvalidSerial("nope".to_owned()),
    "invalid changelog serial \"nope\""
)]
fn test_changelog_request_error_display(#[case] error: ChangelogRequestError, #[case] expected: &str) {
    assert_eq!(error.to_string(), expected);
}

#[test]
fn test_render_last_serial_uses_i8_past_xml_rpc_int_range() {
    assert_eq!(
        render_last_serial_response(42),
        "<?xml version=\"1.0\"?><methodResponse><params><param><value><int>42</int></value></param></params></methodResponse>"
    );
    assert!(render_last_serial_response(i32::MAX as u64 + 1).contains("<i8>2147483648</i8>"));
}

#[test]
fn test_render_changelog_response_preserves_warehouse_tuple_order() {
    let response = render_changelog_response(&[
        ChangelogEntry {
            project: "demo".to_owned(),
            version: Some("1.0".to_owned()),
            timestamp: 1_725_534_675,
            action: "add py3 file demo-1.0.whl".to_owned(),
            serial: 42,
        },
        ChangelogEntry {
            project: "bad<&>\"'\t💩\u{1}\u{7f}\u{fdd0}\u{1fffe}".to_owned(),
            version: None,
            timestamp: -1,
            action: "remove > release".to_owned(),
            serial: i32::MAX as u64 + 1,
        },
        ChangelogEntry {
            project: "large timestamp".to_owned(),
            version: Some(String::new()),
            timestamp: i64::from(i32::MAX) + 1,
            action: String::new(),
            serial: 43,
        },
    ]);
    assert!(response.starts_with("<?xml version=\"1.0\"?><methodResponse>"));
    assert!(
        response
            .contains("<string>demo</string></value><value><string>1.0</string></value><value><int>1725534675</int>")
    );
    assert!(response.contains("<string>bad&lt;&amp;&gt;&quot;&apos;\t💩</string>"));
    assert!(response.contains("<value><nil/></value><value><int>-1</int>"));
    assert!(response.contains("<string>remove &gt; release</string>"));
    assert!(response.contains("<i8>2147483648</i8>"));
    assert!(response.contains("<string>large timestamp</string></value><value><string></string>"));
    assert!(response.contains("<value><i8>2147483648</i8></value><value><string></string>"));
    assert!(response.ends_with("</data></array></value></param></params></methodResponse>"));
}

#[test]
fn test_render_changelog_fault_escapes_message() {
    let response = render_changelog_fault(-32602, "expected <int> & nothing else");
    assert!(response.contains("<name>faultCode</name><value><int>-32602</int>"));
    assert!(response.contains("<string>expected &lt;int&gt; &amp; nothing else</string>"));
}

#[test]
fn test_dispatch_changelog_last_serial_queries_only_the_head() {
    assert_eq!(
        dispatch_changelog_request(
            b"<methodCall><methodName>changelog_last_serial</methodName></methodCall>",
            || Ok::<_, ()>(42),
            |_, _| -> Result<ChangelogPage, ()> { panic!("since query must not run") },
        )
        .unwrap(),
        render_last_serial_response(42)
    );
}

#[test]
fn test_dispatch_changelog_since_serial_passes_the_warehouse_limit() {
    let expected = ChangelogEntry {
        project: "demo".to_owned(),
        version: Some("1.0".to_owned()),
        timestamp: 1_725_534_675,
        action: "add source file demo-1.0.tar.gz".to_owned(),
        serial: 43,
    };
    assert_eq!(
        dispatch_changelog_request(
            b"<methodCall><methodName>changelog_since_serial</methodName><params><param><value><int>42</int></value></param></params></methodCall>",
            || -> Result<u64, ()> { panic!("head query must not run") },
            |serial, limit| {
                assert_eq!(serial, 42);
                assert_eq!(limit, CHANGELOG_PAGE_SIZE);
                Ok::<_, ()>(ChangelogPage::new(serial, expected.serial, vec![expected.clone()]).unwrap())
            },
        )
        .unwrap(),
        render_changelog_response(&[expected])
    );
}

#[rstest::rstest]
#[case::parse(
    b"<methodCall>" as &[u8],
    -32700,
    "parse error; not well formed"
)]
#[case::unknown_method(
    b"<methodCall><methodName>list_packages</methodName></methodCall>",
    -32601,
    "server error; requested method not found"
)]
#[case::invalid_params(
    b"<methodCall><methodName>changelog_since_serial</methodName><params/></methodCall>",
    -32602,
    "server error; invalid method params"
)]
#[case::invalid_serial(
    b"<methodCall><methodName>changelog_since_serial</methodName><params><param><value><int>nope</int></value></param></params></methodCall>",
    -32602,
    "server error; invalid method params"
)]
fn test_dispatch_changelog_request_maps_client_faults(#[case] body: &[u8], #[case] code: i32, #[case] message: &str) {
    assert_eq!(
        dispatch_changelog_request(
            body,
            || -> Result<u64, ()> { panic!("invalid requests must not query the source") },
            |_, _| -> Result<ChangelogPage, ()> { panic!("invalid requests must not query the source") },
        )
        .unwrap(),
        render_changelog_fault(code, message)
    );
}

#[test]
fn test_dispatch_changelog_request_maps_invalid_utf8_to_a_parse_fault() {
    assert_eq!(
        dispatch_changelog_request(
            &[0xff],
            || -> Result<u64, ()> { panic!("invalid requests must not query the source") },
            |_, _| -> Result<ChangelogPage, ()> { panic!("invalid requests must not query the source") },
        )
        .unwrap(),
        render_changelog_fault(-32700, "parse error; not well formed")
    );
}

#[rstest::rstest]
#[case::last_serial(b"<methodCall><methodName>changelog_last_serial</methodName></methodCall>")]
#[case::since_serial(
    b"<methodCall><methodName>changelog_since_serial</methodName><params><param><value><int>42</int></value></param></params></methodCall>"
)]
fn test_dispatch_changelog_request_preserves_source_errors(#[case] body: &[u8]) {
    assert_eq!(
        dispatch_changelog_request(body, || Err::<u64, _>(()), |_, _| Err::<ChangelogPage, _>(())),
        Err(())
    );
}
