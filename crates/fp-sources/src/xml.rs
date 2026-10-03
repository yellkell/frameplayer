//! A small, tolerant XML element tree on top of `quick-xml`, used for
//! WebDAV multistatus, UPnP device descriptions, SOAP and DIDL-Lite.
//!
//! Names are kept without namespace prefixes (`D:href` becomes `href`):
//! servers disagree on prefixes but never on local names in the documents we
//! read. Lookups by name are case-insensitive. Mismatched end tags are
//! tolerated, since some media servers emit sloppy DIDL-Lite.

use crate::error::{Error, Result};
use crate::urlutil::html_unescape;
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

/// One XML element with its attributes, text and children.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Element {
    /// Local name (no namespace prefix).
    pub name: String,
    /// Attributes by local name, values unescaped.
    pub attrs: Vec<(String, String)>,
    /// Concatenated text and CDATA directly inside this element, unescaped.
    pub text: String,
    /// Child elements in document order.
    pub children: Vec<Element>,
}

impl Element {
    /// First child named `name`.
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// Children named `name`.
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> {
        self.children
            .iter()
            .filter(move |c| c.name.eq_ignore_ascii_case(name))
    }

    /// First descendant (depth first, including `self`) named `name`.
    pub fn find(&self, name: &str) -> Option<&Element> {
        if self.name.eq_ignore_ascii_case(name) {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(name))
    }

    /// Every descendant (depth first, including `self`) named `name`.
    pub fn find_all<'a>(&'a self, name: &str, out: &mut Vec<&'a Element>) {
        if self.name.eq_ignore_ascii_case(name) {
            out.push(self);
        }
        for c in &self.children {
            c.find_all(name, out);
        }
    }

    /// Attribute value by local name.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Trimmed text of the first child named `name`, when not empty.
    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name)
            .map(|c| c.text.trim())
            .filter(|t| !t.is_empty())
    }
}

fn local(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    match s.rsplit_once(':') {
        Some((_, l)) => l.to_string(),
        None => s.into_owned(),
    }
}

fn element_from(start: &BytesStart<'_>) -> Element {
    let mut attrs = Vec::new();
    for a in start.attributes().with_checks(false).flatten() {
        let key = local(a.key.as_ref());
        // Fall back to the raw value when it contains a bad escape.
        let value = match a.unescape_value() {
            Ok(v) => v.into_owned(),
            Err(_) => html_unescape(&String::from_utf8_lossy(&a.value)),
        };
        attrs.push((key, value));
    }
    Element {
        name: local(start.name().as_ref()),
        attrs,
        ..Element::default()
    }
}

/// Parses a document. The returned element is a nameless root whose
/// children are the document's top-level elements.
pub(crate) fn parse(xml: &str) -> Result<Element> {
    let mut reader = Reader::from_str(xml);
    let config = reader.config_mut();
    config.check_end_names = false;
    config.expand_empty_elements = false;
    let mut stack: Vec<Element> = vec![Element::default()];
    loop {
        let event = reader
            .read_event()
            .map_err(|e| Error::parse("XML", format!("{e} at byte {}", reader.error_position())))?;
        match event {
            Event::Start(s) => stack.push(element_from(&s)),
            Event::Empty(s) => {
                let el = element_from(&s);
                if let Some(top) = stack.last_mut() {
                    top.children.push(el);
                }
            }
            Event::End(_) => {
                // A stray end tag at the root is ignored.
                if stack.len() > 1 {
                    if let Some(done) = stack.pop() {
                        if let Some(top) = stack.last_mut() {
                            top.children.push(done);
                        }
                    }
                }
            }
            Event::Text(t) => {
                let raw = t.decode().map_err(|e| Error::parse("XML", e))?;
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&html_unescape(&raw));
                }
            }
            Event::CData(c) => {
                let raw = c.decode().map_err(|e| Error::parse("XML", e))?;
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&raw);
                }
            }
            Event::GeneralRef(r) => {
                let name = r.decode().map_err(|e| Error::parse("XML", e))?;
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&html_unescape(&format!("&{name};")));
                }
            }
            Event::Eof => break,
            Event::Decl(_) | Event::PI(_) | Event::Comment(_) | Event::DocType(_) => {}
        }
    }
    // Close elements left open by a truncated document.
    while stack.len() > 1 {
        if let Some(done) = stack.pop() {
            if let Some(top) = stack.last_mut() {
                top.children.push(done);
            }
        }
    }
    let root = stack.pop().unwrap_or_default();
    if root.children.is_empty() {
        return Err(Error::parse("XML", "no elements in document"));
    }
    Ok(root)
}

/// Escapes text for inclusion in an XML element or attribute.
pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_namespaces_text_and_entities() {
        let doc = parse(
            r#"<?xml version="1.0"?>
            <D:multistatus xmlns:D="DAV:"><D:response>
              <D:href>/a%20b/</D:href>
              <D:x a:k="v &amp; w"/>
              <t>Tom &amp; Jerry &#x263A; <![CDATA[<raw>]]></t>
            </D:response></D:multistatus>"#,
        )
        .unwrap();
        let ms = doc.child("multistatus").unwrap();
        let r = ms.child("RESPONSE").unwrap();
        assert_eq!(r.child_text("href"), Some("/a%20b/"));
        assert_eq!(r.child("x").unwrap().attr("k"), Some("v & w"));
        assert_eq!(r.child_text("t"), Some("Tom & Jerry \u{263a} <raw>"));
        assert!(doc.find("href").is_some());
        let mut all = Vec::new();
        doc.find_all("response", &mut all);
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn tolerates_sloppy_documents() {
        let doc = parse("<a><b>x</c><d/></a>").unwrap();
        assert!(doc.find("d").is_some());
        let doc = parse("<a><b>truncated").unwrap();
        assert_eq!(doc.find("b").unwrap().text, "truncated");
        assert!(parse("").is_err());
        assert!(parse("just text").is_err());
    }

    #[test]
    fn escapes() {
        assert_eq!(escape(r#"<a & "b">"#), "&lt;a &amp; &quot;b&quot;&gt;");
    }
}
