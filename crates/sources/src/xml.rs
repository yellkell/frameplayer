//! A tiny namespace-agnostic XML DOM on top of quick-xml.
//!
//! WebDAV multistatus, UPnP device descriptions, SOAP envelopes, DIDL-Lite
//! and DASH MPDs are all small documents that are easiest to interpret as a
//! tree. Element and attribute names keep only their local part (prefixes
//! are dropped), which is how every one of those formats is used in
//! practice: servers disagree wildly on prefixes but not on local names.

use crate::error::{Result, SourceError};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Element {
    /// Local name (no namespace prefix).
    pub name: String,
    /// Attributes by local name, unescaped.
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
    /// Concatenated, unescaped text content directly inside this element.
    pub text: String,
}

impl Element {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// First direct child with the given local name (case-insensitive).
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children
            .iter()
            .filter(move |c| c.name.eq_ignore_ascii_case(name))
    }

    /// Trimmed text of the first direct child with this name.
    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.trim())
    }

    /// Depth-first search for the first descendant (or self) with this name.
    pub fn find(&self, name: &str) -> Option<&Element> {
        if self.name.eq_ignore_ascii_case(name) {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(name))
    }

    /// Follow a path of child names, e.g. `["device", "serviceList"]`.
    pub fn path(&self, names: &[&str]) -> Option<&Element> {
        names.iter().try_fold(self, |e, n| e.child(n))
    }
}

fn local(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    match s.rfind(':') {
        Some(i) => s[i + 1..].to_string(),
        None => s.into_owned(),
    }
}

fn start_element(e: &BytesStart<'_>) -> Result<Element> {
    let mut el = Element {
        name: local(e.name().as_ref()),
        ..Default::default()
    };
    for a in e.attributes().with_checks(false) {
        let a = a.map_err(|e| SourceError::Parse(format!("xml attribute: {e}")))?;
        let key = local(a.key.as_ref());
        if a.key.as_ref().starts_with(b"xmlns") {
            continue;
        }
        let v = a
            .unescape_value()
            .map(|v| v.into_owned())
            .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned());
        el.attrs.push((key, v));
    }
    Ok(el)
}

/// Parse a document and return its root element.
pub fn parse(input: &str) -> Result<Element> {
    let mut reader = Reader::from_str(input);
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;

    loop {
        match reader.read_event()? {
            Event::Start(e) => stack.push(start_element(&e)?),
            Event::Empty(e) => {
                let el = start_element(&e)?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(el),
                    None => root = root.or(Some(el)),
                }
            }
            Event::End(_) => {
                if let Some(el) = stack.pop() {
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(el),
                        None => root = root.or(Some(el)),
                    }
                }
            }
            Event::Text(t) => {
                if let Some(top) = stack.last_mut() {
                    let s = t
                        .unescape()
                        .map(|s| s.into_owned())
                        .unwrap_or_else(|_| String::from_utf8_lossy(&t).into_owned());
                    top.text.push_str(&s);
                }
            }
            Event::CData(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&String::from_utf8_lossy(&t));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    // Unclosed elements (truncated documents): fold them up so callers still
    // get whatever was parsed.
    while let Some(el) = stack.pop() {
        match stack.last_mut() {
            Some(parent) => parent.children.push(el),
            None => root = root.or(Some(el)),
        }
    }
    root.ok_or_else(|| SourceError::Parse("empty XML document".into()))
}

/// Escape text for inclusion in an XML body.
pub fn escape(s: &str) -> String {
    quick_xml::escape::escape(s).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_namespaces_and_entities() {
        let doc = r#"<?xml version="1.0"?>
            <D:multistatus xmlns:D="DAV:"><D:response><D:href>/a%20b/</D:href>
            <D:prop a:x="1&amp;2" xmlns:a="urn:a"><D:displayname>A &amp; B</D:displayname><D:empty/></D:prop>
            </D:response></D:multistatus>"#;
        let root = parse(doc).unwrap();
        assert_eq!(root.name, "multistatus");
        let prop = root.path(&["response", "prop"]).unwrap();
        assert_eq!(prop.attr("x"), Some("1&2"));
        assert_eq!(prop.child_text("displayname"), Some("A & B"));
        assert!(prop.child("empty").is_some());
        assert_eq!(root.find("href").unwrap().text, "/a%20b/");
    }

    #[test]
    fn cdata_and_truncated() {
        let root = parse("<a><b><![CDATA[x<y]]></b><c>").unwrap();
        assert_eq!(root.child_text("b"), Some("x<y"));
        assert!(root.child("c").is_some());
    }
}
