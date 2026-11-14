//! Reading the server's own HTML forms, as a browser would submit them.
//!
//! The Login flow's pages and Consent are plain forms (no script decides what is
//! posted), so a form's fields are all a client needs. This reads only what our
//! pages use: `<form>`, `<input>` and `<button>` tags and their attributes.

use anyhow::{Context, Result, bail};

/// One `<form>` and the controls inside it, in document order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub method: String,
    pub action: String,
    pub controls: Vec<Control>,
}

/// An `<input>` or a `<button>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Control {
    pub tag: Tag,
    /// `type`, lower-cased; `submit` for a button without one.
    pub kind: String,
    pub name: Option<String>,
    pub value: Option<String>,
    pub checked: bool,
    pub disabled: bool,
    /// A submit button's `formaction`, which overrides the form's `action`.
    pub formaction: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Input,
    Button,
}

impl Form {
    /// The form whose `action` is `action`.
    pub fn find(html: &str, action: &str) -> Result<Form> {
        forms(html)
            .into_iter()
            .find(|f| f.action == action)
            .with_context(|| format!("no form posting to {action} on the page"))
    }

    /// The boxes named `name`: (value, checked), skipping disabled ones.
    pub fn checkboxes(&self, name: &str) -> Vec<(String, bool)> {
        self.controls
            .iter()
            .filter(|c| c.kind == "checkbox" && !c.disabled && c.name.as_deref() == Some(name))
            .map(|c| (c.value.clone().unwrap_or_else(|| "on".into()), c.checked))
            .collect()
    }

    /// What pressing the submit button `name=value` posts, with exactly the
    /// boxes named `boxes` that are in `tick` ticked (and no others of them).
    /// Hidden inputs and other fields go as the page has them. Fails if the
    /// button or one of the values to tick isn't on the form.
    pub fn submit(
        &self,
        button: (&str, &str),
        boxes: &str,
        tick: &[&str],
    ) -> Result<Vec<(String, String)>> {
        let mut pairs = self.fields(&[(boxes, tick)])?;
        if !self.controls.iter().any(|c| {
            c.kind == "submit"
                && c.name.as_deref() == Some(button.0)
                && c.value.as_deref() == Some(button.1)
        }) {
            bail!("the form has no {}={} button", button.0, button.1);
        }
        pairs.push((button.0.to_string(), button.1.to_string()));
        Ok(pairs)
    }

    /// The form's fields with exactly `tick`'s values ticked in each named
    /// group of boxes, and no button: what a `formaction` button posts.
    pub fn fields(&self, groups: &[(&str, &[&str])]) -> Result<Vec<(String, String)>> {
        for (boxes, tick) in groups {
            let offered = self.checkboxes(boxes);
            for value in *tick {
                if !offered.iter().any(|(v, _)| v == value) {
                    bail!(
                        "the form offers no {boxes} box with value {value:?} (it offers {:?})",
                        offered.iter().map(|(v, _)| v).collect::<Vec<_>>()
                    );
                }
            }
        }
        let group = |name: &str| groups.iter().find(|(g, _)| *g == name).map(|(_, t)| *t);
        let mut pairs = Vec::new();
        for c in &self.controls {
            let Some(name) = &c.name else { continue };
            if c.disabled {
                continue;
            }
            let value = c.value.clone();
            match c.kind.as_str() {
                "submit" | "button" | "reset" | "image" => {}
                "checkbox" | "radio" => {
                    let value = value.unwrap_or_else(|| "on".into());
                    let on = match group(name) {
                        Some(tick) => tick.contains(&value.as_str()),
                        None => c.checked,
                    };
                    if on {
                        pairs.push((name.clone(), value));
                    }
                }
                _ => pairs.push((name.clone(), value.unwrap_or_default())),
            }
        }
        Ok(pairs)
    }
}

/// Every form on the page.
pub fn forms(html: &str) -> Vec<Form> {
    let mut forms = Vec::new();
    let mut current: Option<Form> = None;
    for tag in tags(html) {
        match tag.name.as_str() {
            "form" if !tag.closing => {
                current = Some(Form {
                    method: tag.attr("method").unwrap_or("get").to_ascii_lowercase(),
                    action: tag.attr("action").unwrap_or_default().to_string(),
                    controls: Vec::new(),
                });
            }
            "form" => forms.extend(current.take()),
            "input" | "button" if !tag.closing => {
                if let Some(form) = current.as_mut() {
                    let button = tag.name == "button";
                    let default_kind = if button { "submit" } else { "text" };
                    form.controls.push(Control {
                        tag: if button { Tag::Button } else { Tag::Input },
                        kind: tag
                            .attr("type")
                            .unwrap_or(default_kind)
                            .to_ascii_lowercase(),
                        name: tag.attr("name").map(str::to_string),
                        value: tag.attr("value").map(str::to_string),
                        checked: tag.has("checked"),
                        disabled: tag.has("disabled"),
                        formaction: tag.attr("formaction").map(str::to_string),
                    });
                }
            }
            _ => {}
        }
    }
    forms.extend(current);
    forms
}

struct RawTag {
    name: String,
    closing: bool,
    attrs: Vec<(String, Option<String>)>,
}

impl RawTag {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_deref().unwrap_or_default())
    }

    fn has(&self, name: &str) -> bool {
        self.attrs.iter().any(|(k, _)| k == name)
    }
}

/// The start and end tags of `html`, skipping comments, doctype, and the
/// contents of `<script>` and `<style>`.
fn tags(html: &str) -> Vec<RawTag> {
    let b = html.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = html[i..].find('<') {
        i += off;
        if html[i..].starts_with("<!--") {
            i = html[i..].find("-->").map_or(b.len(), |e| i + e + 3);
            continue;
        }
        if html[i..].starts_with("<!") || html[i..].starts_with("<?") {
            i = html[i..].find('>').map_or(b.len(), |e| i + e + 1);
            continue;
        }
        let (tag, end) = parse_tag(html, i + 1);
        i = end;
        if let Some(tag) = tag {
            let raw_text = !tag.closing && (tag.name == "script" || tag.name == "style");
            let name = tag.name.clone();
            out.push(tag);
            if raw_text {
                let close = format!("</{name}");
                i = html[i..]
                    .to_ascii_lowercase()
                    .find(&close)
                    .map_or(b.len(), |e| i + e);
            }
        }
    }
    out
}

/// Parses the tag starting right after a `<` at `start`; returns it (if it is
/// one) and the index after its `>`.
fn parse_tag(html: &str, start: usize) -> (Option<RawTag>, usize) {
    let b = html.as_bytes();
    let mut i = start;
    let closing = b.get(i) == Some(&b'/');
    if closing {
        i += 1;
    }
    let name_start = i;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'-') {
        i += 1;
    }
    if i == name_start {
        // A lone "<" in text.
        return (None, start);
    }
    let name = html[name_start..i].to_ascii_lowercase();
    let mut attrs = Vec::new();
    loop {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        if b[i] == b'>' {
            i += 1;
            break;
        }
        let key_start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'=' | b'>' | b'/') {
            i += 1;
        }
        let key = html[key_start..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if b.get(i) != Some(&b'=') {
            attrs.push((key, None));
            continue;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let value = match b.get(i) {
            Some(&q @ (b'"' | b'\'')) => {
                let vs = i + 1;
                let ve = html[vs..].find(q as char).map_or(b.len(), |e| vs + e);
                i = (ve + 1).min(b.len());
                &html[vs..ve]
            }
            _ => {
                let vs = i;
                while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                    i += 1;
                }
                &html[vs..i]
            }
        };
        attrs.push((key, Some(unescape(value))));
    }
    (
        Some(RawTag {
            name,
            closing,
            attrs,
        }),
        i,
    )
}

/// The character references our pages' escaping produces.
fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONSENT: &str = r#"<!doctype html><html><head><style>form{}</style>
<script src="/x.js"></script></head><body>
<!-- <form action="/commented"> -->
<form method="post" action="/login/abc/consent" class="consent">
  <label><input type="checkbox" checked disabled> Read</label>
  <label><input type="checkbox" id="scope-draft" class="x" name="scope" value="draft" checked> Drafts</label>
  <label><input type="checkbox" name="scope" value="send"> Send</label>
  <input type="hidden" name="t" value="a&amp;b">
  <button type="submit" class="pick" formaction="/login/abc/consent/chats" formmethod="post">Choose chats</button>
  <button type="submit" name="decision" value="deny">Deny</button>
  <button type="submit" name="decision" value="allow" class="primary">Allow</button>
</form>
<form method="post" action=/other><input name=q value=x></form>
</body></html>"#;

    #[test]
    fn reads_every_form_and_its_controls() {
        let all = forms(CONSENT);
        assert_eq!(all.len(), 2, "the commented-out form is not one");
        let consent = &all[0];
        assert_eq!(consent.method, "post");
        assert_eq!(consent.action, "/login/abc/consent");
        assert_eq!(consent.controls.len(), 7);
        assert_eq!(
            consent.controls[4].formaction.as_deref(),
            Some("/login/abc/consent/chats")
        );
        assert_eq!(all[1].action, "/other", "unquoted attributes");
        assert_eq!(all[1].controls[0].value.as_deref(), Some("x"));
    }

    #[test]
    fn checkboxes_skip_disabled_ones() {
        let consent = Form::find(CONSENT, "/login/abc/consent").unwrap();
        assert_eq!(
            consent.checkboxes("scope"),
            vec![("draft".to_string(), true), ("send".to_string(), false)]
        );
    }

    #[test]
    fn submit_ticks_exactly_what_is_asked() {
        let consent = Form::find(CONSENT, "/login/abc/consent").unwrap();
        let read_only = consent.submit(("decision", "allow"), "scope", &[]).unwrap();
        assert_eq!(
            read_only,
            vec![
                ("t".to_string(), "a&b".to_string()),
                ("decision".to_string(), "allow".to_string())
            ],
            "the pre-ticked draft box is unticked; hidden fields go as they are"
        );
        let send = consent
            .submit(("decision", "allow"), "scope", &["send"])
            .unwrap();
        assert!(send.contains(&("scope".to_string(), "send".to_string())));
        assert!(!send.contains(&("scope".to_string(), "draft".to_string())));
    }

    #[test]
    fn submit_refuses_what_the_form_does_not_offer() {
        let consent = Form::find(CONSENT, "/login/abc/consent").unwrap();
        let err = consent
            .submit(("decision", "allow"), "scope", &["organize"])
            .unwrap_err();
        assert!(err.to_string().contains("organize"), "{err}");
        let err = consent
            .submit(("decision", "maybe"), "scope", &[])
            .unwrap_err();
        assert!(err.to_string().contains("decision=maybe"), "{err}");
        assert!(Form::find(CONSENT, "/nowhere").is_err());
    }
}
