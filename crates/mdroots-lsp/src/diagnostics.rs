//! Facade diagnostics -> LSP diagnostics, and the `mdroots.diagnostics`
//! setting.

use std::path::Path;

use lsp_types::{DiagnosticRelatedInformation, DiagnosticSeverity, Location, NumberOrString};
use mdroots::syntax::{LineIndex, PositionEncoding};
use mdroots::{DiagCode, Diagnostic, Severity};
use serde_json::Value;

use crate::position;
use crate::uri;

/// The `mdroots.diagnostics` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Setting {
    /// The root's policy decides (also when the setting is missing).
    #[default]
    Auto,
    /// Publish nothing.
    Off,
    /// The severity of broken links and anchors.
    Broken(Severity),
}

impl Setting {
    /// From `workspace/didChangeConfiguration` `params.settings`; an
    /// unknown or missing value is [`Setting::Auto`].
    pub(crate) fn from_settings(settings: &Value) -> Setting {
        match settings
            .pointer("/mdroots/diagnostics")
            .and_then(Value::as_str)
        {
            Some("off") => Setting::Off,
            Some("hint") => Setting::Broken(Severity::Hint),
            Some("warn") => Setting::Broken(Severity::Warning),
            Some("error") => Setting::Broken(Severity::Error),
            _ => Setting::Auto,
        }
    }

    /// Applies the setting to the facade's diagnostics of one document.
    pub(crate) fn apply(self, mut diags: Vec<Diagnostic>) -> Vec<Diagnostic> {
        match self {
            Setting::Auto => {}
            Setting::Off => diags.clear(),
            Setting::Broken(s) => {
                for d in &mut diags {
                    if matches!(d.code, DiagCode::BrokenLink | DiagCode::BrokenAnchor) {
                        d.severity = s;
                    }
                }
            }
        }
        diags
    }
}

/// Lowercase-hyphenated name of a diagnostic code.
pub(crate) fn code_name(c: DiagCode) -> &'static str {
    match c {
        DiagCode::BrokenLink => "broken-link",
        DiagCode::BrokenAnchor => "broken-anchor",
        DiagCode::AmbiguousLink => "ambiguous-link",
        DiagCode::NotInWorkingSet => "not-in-working-set",
        DiagCode::InvalidFrontmatter => "invalid-frontmatter",
        _ => "unknown",
    }
}

fn severity(s: Severity) -> DiagnosticSeverity {
    match s {
        Severity::Error => DiagnosticSeverity::ERROR,
        Severity::Warning => DiagnosticSeverity::WARNING,
        Severity::Info => DiagnosticSeverity::INFORMATION,
        Severity::Hint => DiagnosticSeverity::HINT,
    }
}

/// One LSP diagnostic; `index` is over `text`, the text the diagnostic was
/// computed on; `root` resolves the root-relative related paths.
pub(crate) fn to_lsp(
    d: &Diagnostic,
    index: &LineIndex,
    text: &str,
    enc: PositionEncoding,
    root: &Path,
) -> lsp_types::Diagnostic {
    let related: Vec<_> = d
        .related
        .iter()
        .filter_map(|rel| {
            Some(DiagnosticRelatedInformation {
                location: Location {
                    uri: uri::from_path(&root.join(rel))?,
                    range: lsp_types::Range::default(),
                },
                message: format!("candidate: {rel}"),
            })
        })
        .collect();
    lsp_types::Diagnostic {
        range: position::range(index, text, d.range.clone(), enc),
        severity: Some(severity(d.severity)),
        code: Some(NumberOrString::String(code_name(d.code).to_owned())),
        source: Some("mdroots".to_owned()),
        message: d.message.clone(),
        related_information: (!related.is_empty()).then_some(related),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_the_setting() {
        let s = |v: Value| Setting::from_settings(&json!({ "mdroots": { "diagnostics": v } }));
        assert_eq!(s(json!("off")), Setting::Off);
        assert_eq!(s(json!("hint")), Setting::Broken(Severity::Hint));
        assert_eq!(s(json!("warn")), Setting::Broken(Severity::Warning));
        assert_eq!(s(json!("error")), Setting::Broken(Severity::Error));
        assert_eq!(s(json!("auto")), Setting::Auto);
        assert_eq!(Setting::from_settings(&json!({})), Setting::Auto);
    }
}
