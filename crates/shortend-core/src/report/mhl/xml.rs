//! Minimal XML plumbing for the manifests: escaping for the hand-written (streamed) output and
//! a small element walker over quick-xml for reading.

use std::io::BufRead;

use quick_xml::events::Event;

/// Escapes text or attribute content. Control characters other than tab/LF/CR cannot appear
/// in XML 1.0 at all; they are replaced so a hostile filename cannot break the document.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out
}

/// One step of a document walk.
pub enum Node<'a> {
    /// An element opened. `path` is the stack of local names including this one.
    Open {
        path: &'a [String],
        attrs: &'a [(String, String)],
    },
    /// An element closed. `text` is its direct text content (entities resolved, trimmed).
    Close {
        path: &'a [String],
        attrs: &'a [(String, String)],
        text: &'a str,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct XmlError(pub String);

/// Walks a document, calling `f` for every element open/close. Namespaces are ignored (local
/// names only), which is how the reference implementation reads both MHL formats.
pub fn walk<R: BufRead>(
    reader: R,
    mut f: impl FnMut(Node<'_>) -> Result<(), XmlError>,
) -> Result<(), XmlError> {
    let mut r = quick_xml::Reader::from_reader(reader);
    let mut buf = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut attr_stack: Vec<Vec<(String, String)>> = Vec::new();
    let mut text = String::new();
    let err = |e: &dyn std::fmt::Display, r: &quick_xml::Reader<R>| {
        XmlError(format!("XML error at byte {}: {e}", r.buffer_position()))
    };
    loop {
        buf.clear();
        let ev = r.read_event_into(&mut buf).map_err(|e| err(&e, &r))?;
        match ev {
            Event::Start(ref s) | Event::Empty(ref s) => {
                let name = s.local_name().as_ref().to_owned();
                let mut attrs = Vec::new();
                for a in s.attributes() {
                    let a = a.map_err(|e| err(&e, &r))?;
                    let key = a.key.local_name().as_ref().to_owned();
                    let value = a
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map_err(|e| err(&e, &r))?
                        .into_owned();
                    attrs.push((key, value));
                }
                path.push(name);
                attr_stack.push(attrs);
                text.clear();
                f(Node::Open {
                    path: &path,
                    attrs: attr_stack.last().map_or(&[][..], Vec::as_slice),
                })?;
                if matches!(ev, Event::Empty(_)) {
                    close(&mut path, &mut attr_stack, &mut text, &mut f)?;
                }
            }
            Event::End(_) => close(&mut path, &mut attr_stack, &mut text, &mut f)?,
            Event::Text(t) => text.push_str(&t.xml10_content()),
            Event::CData(t) => text.push_str(&t.xml10_content()),
            Event::GeneralRef(g) => {
                if let Some(c) = g.resolve_char_ref().map_err(|e| err(&e, &r))? {
                    text.push(c);
                } else {
                    let name = g.xml10_content();
                    match quick_xml::escape::resolve_predefined_entity(&name) {
                        Some(v) => text.push_str(v),
                        None => return Err(XmlError(format!("unknown entity &{name};"))),
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if path.is_empty() {
        Ok(())
    } else {
        Err(XmlError("document ended inside an element".into()))
    }
}

fn close(
    path: &mut Vec<String>,
    attr_stack: &mut Vec<Vec<(String, String)>>,
    text: &mut String,
    f: &mut impl FnMut(Node<'_>) -> Result<(), XmlError>,
) -> Result<(), XmlError> {
    let attrs = attr_stack.pop().unwrap_or_default();
    let t = std::mem::take(text);
    f(Node::Close {
        path,
        attrs: &attrs,
        text: t.trim(),
    })?;
    path.pop();
    Ok(())
}

pub fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping() {
        assert_eq!(
            esc("a<b & \"c\" 'd'>"),
            "a&lt;b &amp; &quot;c&quot; &apos;d&apos;&gt;"
        );
        assert_eq!(esc("bell\u{7}"), "bell\u{FFFD}");
        assert_eq!(esc("Ünïcödé — 名前"), "Ünïcödé — 名前");
    }

    #[test]
    fn walk_resolves_entities_and_attrs() {
        let doc = br#"<?xml version="1.0"?><a xmlns="urn:x"><b k="1 &amp; 2">x &lt; y &#x540D;</b><c/></a>"#;
        let mut seen = Vec::new();
        walk(&doc[..], |n| {
            if let Node::Close { path, attrs, text } = n {
                seen.push((path.join("/"), attrs.to_vec(), text.to_owned()));
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(
            seen[0],
            (
                "a/b".into(),
                vec![("k".into(), "1 & 2".into())],
                "x < y 名".into()
            )
        );
        assert_eq!(seen[1].0, "a/c");
        assert_eq!(seen[2].0, "a");
    }
}
