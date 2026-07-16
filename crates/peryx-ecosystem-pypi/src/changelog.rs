//! Warehouse-compatible XML-RPC types for the `PyPI` changelog methods.

use std::fmt::Write as _;

use xml::reader::{EventReader, XmlEvent};

use crate::serial::ChangelogPage;

/// Warehouse's maximum `changelog_since_serial` result size.
pub const CHANGELOG_PAGE_SIZE: usize = 50_000;

/// A supported Warehouse mirroring request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangelogRequest {
    /// Return the newest journal serial.
    LastSerial,
    /// Return journal records after this serial.
    SinceSerial(i64),
}

/// One Warehouse `changelog_since_serial` result row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogEntry {
    pub project: String,
    pub version: Option<String>,
    pub timestamp: i64,
    pub action: String,
    pub serial: u64,
}

/// Why an XML-RPC request cannot be dispatched to a supported changelog method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangelogRequestError {
    InvalidUtf8,
    MalformedXml,
    InvalidShape(&'static str),
    UnsupportedMethod(String),
    InvalidSerial(String),
}

impl std::fmt::Display for ChangelogRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUtf8 => formatter.write_str("XML-RPC request is not UTF-8"),
            Self::MalformedXml => formatter.write_str("XML-RPC request is malformed"),
            Self::InvalidShape(reason) => write!(formatter, "invalid XML-RPC request: {reason}"),
            Self::UnsupportedMethod(method) => write!(formatter, "unsupported XML-RPC method {method:?}"),
            Self::InvalidSerial(value) => write!(formatter, "invalid changelog serial {value:?}"),
        }
    }
}

impl std::error::Error for ChangelogRequestError {}

/// Parse the two Warehouse XML-RPC methods used for changelog mirroring.
///
/// # Errors
/// Returns [`ChangelogRequestError`] for invalid XML, an unsupported method, or invalid parameters.
pub fn parse_changelog_request(body: &[u8]) -> Result<ChangelogRequest, ChangelogRequestError> {
    let body = std::str::from_utf8(body).map_err(|_| ChangelogRequestError::InvalidUtf8)?;
    let mut roots = 0;
    let mut method_count = 0;
    let mut method = String::new();
    let mut params = 0;
    let mut values = 0;
    let mut value_children = 0;
    let mut integer_count = 0;
    let mut integer = String::new();
    let mut stack = Vec::new();
    for event in EventReader::new(body.as_bytes()) {
        match event.map_err(|_| ChangelogRequestError::MalformedXml)? {
            XmlEvent::StartElement { name, .. } => {
                stack.push(name.local_name);
                match stack.join("/").as_str() {
                    "methodCall" => roots += 1,
                    "methodCall/methodName" => method_count += 1,
                    "methodCall/params/param" => params += 1,
                    "methodCall/params/param/value" => values += 1,
                    "methodCall/params/param/value/int"
                    | "methodCall/params/param/value/i4"
                    | "methodCall/params/param/value/i8" => {
                        integer_count += 1;
                        value_children += 1;
                    }
                    _ if stack.len() == 5 && stack[..4] == ["methodCall", "params", "param", "value"] => {
                        value_children += 1;
                    }
                    _ => {}
                }
            }
            XmlEvent::Characters(text) | XmlEvent::CData(text) | XmlEvent::Whitespace(text) => {
                match stack.join("/").as_str() {
                    "methodCall/methodName" => method.push_str(&text),
                    "methodCall/params/param/value/int"
                    | "methodCall/params/param/value/i4"
                    | "methodCall/params/param/value/i8" => integer.push_str(&text),
                    _ => {}
                }
            }
            XmlEvent::EndElement { .. } => {
                stack.pop();
            }
            _ => {}
        }
    }
    if roots != 1 || method_count != 1 {
        return Err(ChangelogRequestError::InvalidShape(
            "expected one methodCall and methodName",
        ));
    }
    let method = method.trim();
    match method {
        "changelog_last_serial" if params == 0 => Ok(ChangelogRequest::LastSerial),
        "changelog_last_serial" => Err(ChangelogRequestError::InvalidShape(
            "changelog_last_serial takes no parameters",
        )),
        "changelog_since_serial" if params == 1 => parse_since_serial(values, value_children, integer_count, &integer),
        "changelog_since_serial" => Err(ChangelogRequestError::InvalidShape(
            "changelog_since_serial takes one integer",
        )),
        _ => Err(ChangelogRequestError::UnsupportedMethod(method.to_owned())),
    }
}

/// Render a `changelog_last_serial` result.
#[must_use]
pub fn render_last_serial_response(serial: u64) -> String {
    format!(
        "<?xml version=\"1.0\"?><methodResponse><params><param><value>{}</value></param></params></methodResponse>",
        integer(serial)
    )
}

/// Render `changelog_since_serial` rows in Warehouse tuple order.
#[must_use]
pub fn render_changelog_response(entries: &[ChangelogEntry]) -> String {
    let mut response = String::from("<?xml version=\"1.0\"?><methodResponse><params><param><value><array><data>");
    for entry in entries {
        response.push_str("<value><array><data>");
        string_value(&mut response, &entry.project);
        optional_string_value(&mut response, entry.version.as_deref());
        signed_integer_value(&mut response, entry.timestamp);
        string_value(&mut response, &entry.action);
        response.push_str("<value>");
        response.push_str(&integer(entry.serial));
        response.push_str("</value></data></array></value>");
    }
    response.push_str("</data></array></value></param></params></methodResponse>");
    response
}

/// Render an XML-RPC fault document.
#[must_use]
pub fn render_changelog_fault(code: i32, message: &str) -> String {
    let mut response = format!(
        "<?xml version=\"1.0\"?><methodResponse><fault><value><struct><member><name>faultCode</name><value><int>{code}</int></value></member><member><name>faultString</name>"
    );
    string_value(&mut response, message);
    response.push_str("</member></struct></value></fault></methodResponse>");
    response
}

/// Dispatch a changelog request against storage callbacks and render its XML-RPC response.
///
/// Client errors become XML-RPC faults. Storage errors remain typed so the transport can log them
/// before returning an opaque application fault. The transport is also responsible for limiting
/// the request body before calling this function.
///
/// # Errors
/// Returns the storage callback's error when a valid request cannot be served.
pub fn dispatch_changelog_request<E>(
    body: &[u8],
    last_serial: impl FnOnce() -> Result<u64, E>,
    since_serial: impl FnOnce(i64, usize) -> Result<ChangelogPage, E>,
) -> Result<String, E> {
    match parse_changelog_request(body) {
        Ok(ChangelogRequest::LastSerial) => last_serial().map(render_last_serial_response),
        Ok(ChangelogRequest::SinceSerial(serial)) => {
            since_serial(serial, CHANGELOG_PAGE_SIZE).map(|page| render_changelog_response(page.entries()))
        }
        Err(ChangelogRequestError::InvalidUtf8 | ChangelogRequestError::MalformedXml) => {
            Ok(render_changelog_fault(-32700, "parse error; not well formed"))
        }
        Err(ChangelogRequestError::UnsupportedMethod(_)) => Ok(render_changelog_fault(
            -32601,
            "server error; requested method not found",
        )),
        Err(ChangelogRequestError::InvalidShape(_) | ChangelogRequestError::InvalidSerial(_)) => {
            Ok(render_changelog_fault(-32602, "server error; invalid method params"))
        }
    }
}

fn parse_since_serial(
    values: usize,
    value_children: usize,
    integer_count: usize,
    integer: &str,
) -> Result<ChangelogRequest, ChangelogRequestError> {
    if values != 1 || value_children != 1 || integer_count != 1 {
        return Err(ChangelogRequestError::InvalidShape(
            "changelog_since_serial takes one integer",
        ));
    }
    let value = integer.trim();
    value
        .parse()
        .map(ChangelogRequest::SinceSerial)
        .map_err(|_| ChangelogRequestError::InvalidSerial(value.to_owned()))
}

fn integer(value: u64) -> String {
    if i32::try_from(value).is_ok() {
        format!("<int>{value}</int>")
    } else {
        format!("<i8>{value}</i8>")
    }
}

fn signed_integer_value(response: &mut String, value: i64) {
    let tag = if i32::try_from(value).is_ok() { "int" } else { "i8" };
    write!(response, "<value><{tag}>{value}</{tag}></value>").expect("writing to a string cannot fail");
}

fn optional_string_value(response: &mut String, value: Option<&str>) {
    if let Some(value) = value {
        string_value(response, value);
    } else {
        response.push_str("<value><nil/></value>");
    }
}

fn string_value(response: &mut String, value: &str) {
    response.push_str("<value><string>");
    escape_xml(response, value);
    response.push_str("</string></value>");
}

fn escape_xml(response: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '&' => response.push_str("&amp;"),
            '<' => response.push_str("&lt;"),
            '>' => response.push_str("&gt;"),
            '"' => response.push_str("&quot;"),
            '\'' => response.push_str("&apos;"),
            '\u{0}'..='\u{8}'
            | '\u{b}'..='\u{c}'
            | '\u{e}'..='\u{1f}'
            | '\u{7f}'..='\u{84}'
            | '\u{86}'..='\u{9f}'
            | '\u{fdd0}'..='\u{fddf}' => {}
            character if matches!(character as u32 & 0xffff, 0xfffe | 0xffff) => {}
            character => response.push(character),
        }
    }
}
