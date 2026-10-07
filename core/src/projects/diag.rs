use chrono::{DateTime, Local};
use tower_lsp::lsp_types::*;
use std::fmt::Display;

// Standard MSBuild / dcc32 format:
// <file>(<line>[,<col>]): (error|warning|hint|fatal) <CODE>: <message> [<project>]
const MSBUILD_OUTPUT_REGEX: &str = r"^(?P<file>.*?)[(](?P<line>\d+)(?:,(?P<column>\d+))?[)]:\s+(?P<kind>.*?)\s+(?P<code>[A-Z]\d+):\s+(?P<message>.*?)(?:\s+\[.*\])?$";

// Tail shared by the two native-dcc formats below, starting after the closing
// parenthesis of the line/column notation:
//   [whitespace]<localized_label>: <CODE> <message>[ [<project>]]
//
// The label is the compiler's localized severity word ("Warning:", "Warnung:",
// "Hinweis:", "Fatal Error:"). It is therefore matched as "letters and spaces",
// never by a fixed word list, and the severity is derived from <CODE> alone.
// Delphi 2007 separates the label from the parenthesis by a space, Delphi 12
// glues it on ("...pas(205)Warnung: W1057 ..."), so the separator is optional.
// The label itself is optional too, but it can never start with ':' – that keeps
// this tail disjoint from the MSBuild format above ("...pas(205): warning W1057:"),
// whose code is followed by a colon and can never satisfy "<CODE><space>".
const DCC_NATIVE_TAIL: &str = r"\s*(?:\p{L}[\p{L} ]*)?:\s*(?P<code>[A-Z]\d+)\s+(?P<message>\S.*?)(?:\s+\[[^\]]*\])?\s*$";

// Delphi 2007 / Borland MSBuild wrapper format:
// <target_file> : (warning|error|hint|fatal) : <source_file>(<line>)<tail>
const DELPHI2007_MSBUILD_PREFIX: &str =
    r"^.*?\s+:\s+(?:warning|error|hint|fatal)\s+:\s+(?P<file>.*?)[(](?P<line>\d+)(?:,(?P<column>\d+))?[)]";

// Native compiler output without MSBuild wrapper (Delphi 2007 duplicate line as
// well as the Delphi 12 dcc output MSBuild passes through verbatim), optionally
// indented:
//   <source_file>(<line>[,<col>])<tail>
const DCC_NATIVE_PREFIX: &str =
    r"^\s*(?P<file>\S.*?)[(](?P<line>\d+)(?:,(?P<column>\d+))?[)]";

#[derive(Debug)]
pub enum DiagnosticKind {
    ERROR,
    WARN,
    HINT,
}

impl Display for DiagnosticKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiagnosticKind::ERROR => write!(f, "ERROR"),
            DiagnosticKind::WARN => write!(f, "WARN"),
            DiagnosticKind::HINT => write!(f, "HINT"),
        }
    }
}

pub struct CompilerLineDiagnostic {
    pub time: DateTime<Local>,
    pub file: String,
    pub line: u32,
    pub column: Option<u32>,
    pub message: String,
    pub code: String,
    pub kind: DiagnosticKind,
    pub compiler_name: String,
}

impl Display for CompilerLineDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let time = self.time.format("%H:%M:%S%.3f");
        let kind = &self.kind;
        let code = &self.code;
        let file = &self.file;
        let line = &self.line;
        let message = &self.message;
        if let Some(column) = self.column {
            write!(
                f,
                "{time}: [{kind}][{code}] {file}:{line}:{column} - {message}",
            )
        } else {
            write!(
                f,
                "{time}: [{kind}][{code}] {file}:{line} - {message}"
            )
        }
    }
}

lazy_static::lazy_static! {
    pub static ref COMPILER_OUTPUT_REGEX: regex::Regex = regex::Regex::new(MSBUILD_OUTPUT_REGEX).unwrap();
    static ref DELPHI2007_MSBUILD_OUTPUT_REGEX: regex::Regex =
        regex::Regex::new(&format!("{DELPHI2007_MSBUILD_PREFIX}{DCC_NATIVE_TAIL}")).unwrap();
    static ref DCC_NATIVE_OUTPUT_REGEX: regex::Regex =
        regex::Regex::new(&format!("{DCC_NATIVE_PREFIX}{DCC_NATIVE_TAIL}")).unwrap();
}

fn build_from_captures(captures: regex::Captures, compiler_name: String) -> Option<CompilerLineDiagnostic> {
    let file = captures.name("file")?.as_str().trim().to_string();
    let line_num = captures.name("line")?.as_str().parse().ok()?;
    let column = captures
        .name("column")
        .and_then(|m| m.as_str().parse().ok());
    let message = captures.name("message")?.as_str().to_string();
    let code = captures.name("code")?.as_str().to_string();
    let kind = if code.starts_with('H') {
        DiagnosticKind::HINT
    } else if code.starts_with('W') {
        DiagnosticKind::WARN
    } else {
        DiagnosticKind::ERROR
    };
    Some(CompilerLineDiagnostic {
        time: Local::now(),
        file,
        line: line_num,
        column,
        message,
        code,
        kind,
        compiler_name,
    })
}

impl CompilerLineDiagnostic {
    /// Try to parse a raw compiler output line into a [`CompilerLineDiagnostic`].
    ///
    /// Attempts three formats in order:
    /// 1. Standard MSBuild / dcc32 format
    /// 2. Delphi 2007 Borland.Delphi.Targets MSBuild wrapper
    /// 3. Native dcc output (Delphi 2007 duplicate line, Delphi 12 pass-through)
    ///
    /// The wrapper format is tried before the native one because its line also
    /// ends in the native shape – matching natively first would make the file
    /// capture swallow the `<target> : warning : ` prefix.
    ///
    /// The severity is always derived from the message code, never from the
    /// label, which the compiler emits in the IDE's UI language.
    pub fn from_line(line: &str, compiler_name: String) -> Option<Self> {
        if let Some(captures) = COMPILER_OUTPUT_REGEX.captures(line) {
            return build_from_captures(captures, compiler_name);
        }
        if let Some(captures) = DELPHI2007_MSBUILD_OUTPUT_REGEX.captures(line) {
            return build_from_captures(captures, compiler_name);
        }
        if let Some(captures) = DCC_NATIVE_OUTPUT_REGEX.captures(line) {
            return build_from_captures(captures, compiler_name);
        }
        None
    }
}

impl Into<Diagnostic> for CompilerLineDiagnostic {
    fn into(self) -> Diagnostic {
        return Diagnostic {
            range: Range {
                start: Position {
                    line: self.line.saturating_sub(1),
                    character: self.column.unwrap_or(1).saturating_sub(1),
                },
                end: Position {
                    line: self.line.saturating_sub(1),
                    character: self.column.unwrap_or(1).saturating_sub(1) + 1,
                },
            },
            severity: match self.kind {
                DiagnosticKind::ERROR => Some(DiagnosticSeverity::ERROR),
                DiagnosticKind::WARN => Some(DiagnosticSeverity::WARNING),
                DiagnosticKind::HINT => Some(DiagnosticSeverity::HINT),
            },
            code: Some(NumberOrString::String(self.code.clone())),
            source: Some(self.compiler_name.to_string()),
            message: self.message.clone(),
            ..Default::default()
        };
    }
}
