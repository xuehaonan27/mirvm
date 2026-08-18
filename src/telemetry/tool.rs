//! `mirvm log inspect|export` command implementation.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Map, Value, json};

use super::decode::{
    DecodeOutcome, DecodedEvent, DecodedKind, EventContext, FileReport, Health, decode_file,
};
use super::format::{KIND_ENGINE_CONTEXT, KIND_SYSCALL_ENTER, KIND_SYSCALL_EXIT, SyscallSemantics};

const USAGE: &str = "\
usage:
    mirvm log inspect FILE|SESSION
    mirvm log export FILE|SESSION [--engine ID|unknown] [--producer ID]
                     [--tid ID] [--kind KIND] [--sequence N|START:END]
";

pub(crate) fn main(args: impl Iterator<Item = String>) -> ExitCode {
    let stdout = io::stdout();
    let stderr = io::stderr();
    run(args.collect(), &mut stdout.lock(), &mut stderr.lock())
}

fn run(args: Vec<String>, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode {
    let Some(command) = args.first().map(String::as_str) else {
        let _ = stderr.write_all(USAGE.as_bytes());
        return ExitCode::from(2);
    };
    match command {
        "inspect" if args.len() == 2 => match inspect(Path::new(&args[1]), stdout) {
            Ok(Health::Clean) => ExitCode::SUCCESS,
            Ok(_) => ExitCode::from(1),
            Err(error) => {
                let _ = writeln!(stderr, "mirvm log inspect: {error}");
                ExitCode::from(1)
            }
        },
        "export" if args.len() >= 2 => {
            let filter = match Filter::parse(&args[2..]) {
                Ok(filter) => filter,
                Err(error) => {
                    let _ = writeln!(stderr, "mirvm log export: {error}\n{USAGE}");
                    return ExitCode::from(2);
                }
            };
            match export(Path::new(&args[1]), &filter, stdout) {
                Ok(Health::Clean) => ExitCode::SUCCESS,
                Ok(_) => ExitCode::from(1),
                Err(error) => {
                    let _ = writeln!(stderr, "mirvm log export: {error}");
                    ExitCode::from(1)
                }
            }
        }
        "inspect" | "export" => {
            let _ = stderr.write_all(USAGE.as_bytes());
            ExitCode::from(2)
        }
        _ => {
            let _ = stderr.write_all(USAGE.as_bytes());
            ExitCode::from(2)
        }
    }
}

fn inspect(path: &Path, output: &mut dyn Write) -> Result<Health, String> {
    let files = event_files(path)?;
    let mut outcomes = Vec::with_capacity(files.len());
    for file in files {
        outcomes.push(decode_file(&file, &mut |_| Ok(())).map_err(|error| error.to_string())?);
    }
    require_same_session(&outcomes)?;
    let health = outcomes
        .iter()
        .map(|outcome| outcome.health)
        .max()
        .unwrap_or(Health::Corrupt);
    let value = inspection_json(path, health, &outcomes);
    serde_json::to_writer_pretty(&mut *output, &value)
        .map_err(|error| format!("cannot write inspection JSON: {error}"))?;
    writeln!(output).map_err(|error| format!("cannot finish inspection JSON: {error}"))?;
    Ok(health)
}

fn export(path: &Path, filter: &Filter, output: &mut dyn Write) -> Result<Health, String> {
    let files = event_files(path)?;
    let mut expected_session = None;
    let mut health = Health::Clean;
    for file in files {
        let mut emit = |event: &DecodedEvent| -> io::Result<()> {
            if filter.matches(event) {
                serde_json::to_writer(&mut *output, &event_json(event))
                    .map_err(io::Error::other)?;
                output.write_all(b"\n")?;
            }
            Ok(())
        };
        let outcome = decode_file(&file, &mut emit).map_err(|error| error.to_string())?;
        if let Some(session) = expected_session {
            if outcome.header.session_id != session {
                return Err(format!(
                    "{} belongs to a different capture session",
                    file.display()
                ));
            }
        } else {
            expected_session = Some(outcome.header.session_id);
        }
        health = health.max(outcome.health);
    }
    Ok(health)
}

fn event_files(path: &Path) -> Result<Vec<PathBuf>, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("refusing symlink input {}", path.display()));
    }
    if metadata.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if !metadata.is_dir() {
        return Err(format!(
            "{} is neither a file nor a directory",
            path.display()
        ));
    }
    let mut files = Vec::new();
    let entries = std::fs::read_dir(path)
        .map_err(|error| format!("cannot read session directory {}: {error}", path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "cannot read entry in session directory {}: {error}",
                path.display()
            )
        })?;
        let name = entry.file_name();
        if !is_event_name(&name) {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect {}: {error}", entry.path().display()))?;
        if file_type.is_symlink() {
            return Err(format!(
                "refusing event-file symlink {}",
                entry.path().display()
            ));
        }
        if file_type.is_file() {
            files.push(entry.path());
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(format!(
            "session directory {} contains no events-*.mlog files",
            path.display()
        ));
    }
    Ok(files)
}

fn is_event_name(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with("events-") && (name.ends_with(".mlog") || name.ends_with(".mlog.partial"))
}

fn require_same_session(outcomes: &[DecodeOutcome]) -> Result<(), String> {
    let Some(first) = outcomes.first() else {
        return Err("no event files were decoded".into());
    };
    for outcome in &outcomes[1..] {
        if outcome.header.session_id != first.header.session_id {
            return Err(format!(
                "{} belongs to a different capture session",
                outcome.path.display()
            ));
        }
    }
    Ok(())
}

fn inspection_json(path: &Path, health: Health, outcomes: &[DecodeOutcome]) -> Value {
    let mut totals = FileReport::default();
    let mut files = Vec::with_capacity(outcomes.len());
    for outcome in outcomes {
        merge_report(&mut totals, &outcome.report);
        files.push(json!({
            "path": outcome.path.display().to_string(),
            "status": outcome.health.as_str(),
            "pid": outcome.header.pid.to_string(),
            "process_generation": outcome.header.process_generation.to_string(),
            "segment": outcome.header.segment_number.to_string(),
            "chunks": outcome.report.committed_chunks.to_string(),
            "pages": outcome.report.committed_pages.to_string(),
            "records": outcome.report.records.to_string(),
            "issues": outcome.issues.iter().map(|issue| json!({
                "offset": issue.offset.to_string(),
                "message": issue.message,
            })).collect::<Vec<_>>(),
        }));
    }
    let session_id = outcomes
        .first()
        .map(|outcome| hex_bytes(&outcome.header.session_id))
        .unwrap_or_else(|| "unavailable".into());
    let mut kinds = Map::new();
    for (kind, count) in &totals.kind_counts {
        kinds.insert(kind_name(*kind), Value::String(count.to_string()));
    }
    let mut engines = Map::new();
    for (engine, count) in &totals.engine_counts {
        let name = engine.map_or_else(|| "unknown".into(), |id| id.to_string());
        engines.insert(name, Value::String(count.to_string()));
    }
    json!({
        "format": "mirvm-log-v0",
        "input": path.display().to_string(),
        "status": health.as_str(),
        "session_id": session_id,
        "time_range": "unavailable",
        "files": files,
        "counts": {
            "chunks": totals.committed_chunks.to_string(),
            "pages": totals.committed_pages.to_string(),
            "bytes": totals.committed_bytes.to_string(),
            "records": totals.records.to_string(),
            "producers": totals.producers.len().to_string(),
            "threads": totals.threads.len().to_string(),
            "producer_ends": totals.producer_ends.to_string(),
            "unknown_records": totals.unknown_records.to_string(),
            "sequence_gaps": totals.sequence_gaps.to_string(),
            "incomplete_enters": totals.incomplete_enters.to_string(),
            "orphan_exits": totals.orphan_exits.to_string(),
            "contract_violations": totals.contract_violations.to_string(),
        },
        "kinds": kinds,
        "engines": engines,
    })
}

fn merge_report(total: &mut FileReport, report: &FileReport) {
    total.committed_chunks = total
        .committed_chunks
        .saturating_add(report.committed_chunks);
    total.committed_pages = total.committed_pages.saturating_add(report.committed_pages);
    total.committed_bytes = total.committed_bytes.saturating_add(report.committed_bytes);
    total.records = total.records.saturating_add(report.records);
    total.producer_ends = total.producer_ends.saturating_add(report.producer_ends);
    total.unknown_records = total.unknown_records.saturating_add(report.unknown_records);
    total.sequence_gaps = total.sequence_gaps.saturating_add(report.sequence_gaps);
    total.incomplete_enters = total
        .incomplete_enters
        .saturating_add(report.incomplete_enters);
    total.orphan_exits = total.orphan_exits.saturating_add(report.orphan_exits);
    total.contract_violations = total
        .contract_violations
        .saturating_add(report.contract_violations);
    for (kind, count) in &report.kind_counts {
        *total.kind_counts.entry(*kind).or_default() = total
            .kind_counts
            .get(kind)
            .copied()
            .unwrap_or(0)
            .saturating_add(*count);
    }
    for (engine, count) in &report.engine_counts {
        *total.engine_counts.entry(*engine).or_default() = total
            .engine_counts
            .get(engine)
            .copied()
            .unwrap_or(0)
            .saturating_add(*count);
    }
    total.producers.extend(report.producers.iter().copied());
    total.threads.extend(report.threads.iter().copied());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EngineFilter {
    Id(u64),
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KindFilter {
    Id(u16),
    Unknown,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Filter {
    engine: Option<EngineFilter>,
    producer: Option<u64>,
    tid: Option<u32>,
    kind: Option<KindFilter>,
    sequence: Option<(u64, u64)>,
}

impl Filter {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut filter = Self::default();
        let mut index = 0;
        while index < args.len() {
            let option = args[index].as_str();
            let value = args
                .get(index + 1)
                .ok_or_else(|| format!("{option} needs an argument"))?;
            match option {
                "--engine" if filter.engine.is_none() => {
                    filter.engine = Some(if value == "unknown" {
                        EngineFilter::Unknown
                    } else {
                        EngineFilter::Id(parse_u64(value, "Engine id")?)
                    });
                }
                "--producer" if filter.producer.is_none() => {
                    filter.producer = Some(parse_u64(value, "producer id")?);
                }
                "--tid" if filter.tid.is_none() => {
                    filter.tid = Some(
                        value
                            .parse::<u32>()
                            .map_err(|_| format!("invalid OS tid `{value}`"))?,
                    );
                }
                "--kind" if filter.kind.is_none() => {
                    filter.kind = Some(parse_kind(value)?);
                }
                "--sequence" if filter.sequence.is_none() => {
                    filter.sequence = Some(parse_sequence(value)?);
                }
                _ if option.starts_with('-') => {
                    return Err(format!("unknown or repeated option `{option}`"));
                }
                _ => return Err(format!("unexpected argument `{option}`")),
            }
            index += 2;
        }
        Ok(filter)
    }

    fn matches(&self, event: &DecodedEvent) -> bool {
        if let Some(engine) = self.engine {
            let matches = match engine {
                EngineFilter::Id(id) => event.context.engine_id == Some(id),
                EngineFilter::Unknown => event.context.engine_id.is_none(),
            };
            if !matches {
                return false;
            }
        }
        if self
            .producer
            .is_some_and(|producer| event.context.producer_id != producer)
            || self.tid.is_some_and(|tid| event.context.os_tid != tid)
        {
            return false;
        }
        if let Some(kind) = self.kind {
            let matches = match kind {
                KindFilter::Id(id) => event.kind.kind_id() == id,
                KindFilter::Unknown => matches!(event.kind, DecodedKind::Unknown { .. }),
            };
            if !matches {
                return false;
            }
        }
        self.sequence
            .is_none_or(|(start, end)| (start..end).contains(&event.context.sequence))
    }
}

fn parse_u64(value: &str, what: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("invalid {what} `{value}`"))
}

fn parse_kind(value: &str) -> Result<KindFilter, String> {
    match value {
        "syscall-enter" | "syscall_enter" => Ok(KindFilter::Id(KIND_SYSCALL_ENTER)),
        "syscall-exit" | "syscall_exit" => Ok(KindFilter::Id(KIND_SYSCALL_EXIT)),
        "engine-context" | "engine_context" => Ok(KindFilter::Id(KIND_ENGINE_CONTEXT)),
        "unknown" => Ok(KindFilter::Unknown),
        _ => Err(format!("unknown event kind `{value}`")),
    }
}

fn parse_sequence(value: &str) -> Result<(u64, u64), String> {
    if let Some((start, end)) = value.split_once(':') {
        let start = parse_u64(start, "sequence start")?;
        let end = parse_u64(end, "sequence end")?;
        if start >= end {
            return Err("sequence range must be non-empty and end-exclusive".into());
        }
        Ok((start, end))
    } else {
        let sequence = parse_u64(value, "sequence")?;
        Ok((
            sequence,
            sequence
                .checked_add(1)
                .ok_or_else(|| "sequence value has no exclusive upper bound".to_string())?,
        ))
    }
}

fn event_json(event: &DecodedEvent) -> Value {
    let EventContext {
        session_id,
        pid,
        process_generation,
        producer_id,
        thread_generation,
        os_tid,
        sequence,
        engine_id,
    } = &event.context;
    let mut object = Map::from_iter([
        ("session_id".into(), Value::String(hex_bytes(session_id))),
        ("pid".into(), Value::String(pid.to_string())),
        (
            "process_generation".into(),
            Value::String(process_generation.to_string()),
        ),
        ("producer_id".into(), Value::String(producer_id.to_string())),
        (
            "thread_generation".into(),
            Value::String(thread_generation.to_string()),
        ),
        ("tid".into(), Value::String(os_tid.to_string())),
        ("sequence".into(), Value::String(sequence.to_string())),
        (
            "engine_id".into(),
            engine_id.map_or(Value::Null, |id| Value::String(id.to_string())),
        ),
        ("timestamp".into(), Value::Null),
        ("time_status".into(), Value::String("unavailable".into())),
    ]);
    match &event.kind {
        DecodedKind::SyscallEnter(enter) => {
            object.insert("kind".into(), Value::String("syscall_enter".into()));
            object.insert(
                "semantics".into(),
                Value::String(semantics_name(enter.semantics).into()),
            );
            object.insert("nr".into(), Value::String(enter.nr.to_string()));
            object.insert(
                "args".into(),
                Value::Array(
                    enter
                        .args
                        .iter()
                        .map(|arg| Value::String(format!("0x{arg:016x}")))
                        .collect(),
                ),
            );
        }
        DecodedKind::SyscallExit {
            record,
            enter_sequence,
        } => {
            object.insert("kind".into(), Value::String("syscall_exit".into()));
            object.insert(
                "semantics".into(),
                Value::String(semantics_name(record.semantics).into()),
            );
            object.insert("result".into(), Value::String(record.result.to_string()));
            object.insert(
                "errno".into(),
                record.errno.map_or(Value::Null, |errno| json!(errno)),
            );
            object.insert(
                "enter_sequence".into(),
                enter_sequence.map_or(Value::Null, |value| Value::String(value.to_string())),
            );
            object.insert(
                "pair_status".into(),
                Value::String(
                    if enter_sequence.is_some() {
                        "paired"
                    } else {
                        "incomplete"
                    }
                    .into(),
                ),
            );
        }
        DecodedKind::EngineContext(context) => {
            object.insert("kind".into(), Value::String("engine_context".into()));
            object.insert(
                "new_engine_id".into(),
                Value::String(context.engine_id.to_string()),
            );
        }
        DecodedKind::Unknown {
            kind,
            version,
            flags,
            bytes,
        } => {
            object.insert("kind".into(), Value::String("unknown".into()));
            object.insert("unknown_kind".into(), json!(*kind));
            object.insert("record_version".into(), json!(*version));
            object.insert("flags".into(), Value::String(format!("0x{flags:02x}")));
            object.insert("payload".into(), Value::String(hex_bytes(bytes)));
        }
    }
    Value::Object(object)
}

const fn semantics_name(semantics: SyscallSemantics) -> &'static str {
    match semantics {
        SyscallSemantics::Raw => "raw",
        SyscallSemantics::Libc => "libc",
    }
}

fn kind_name(kind: u16) -> String {
    match kind {
        KIND_SYSCALL_ENTER => "syscall_enter".into(),
        KIND_SYSCALL_EXIT => "syscall_exit".into(),
        KIND_ENGINE_CONTEXT => "engine_context".into(),
        _ => format!("unknown_0x{kind:04x}"),
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    for byte in bytes {
        write!(out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::decode::{DecodedEvent, DecodedKind, EventContext};
    use crate::telemetry::format::{SyscallEnter, SyscallSemantics};

    fn event() -> DecodedEvent {
        DecodedEvent {
            context: EventContext {
                session_id: [1; 16],
                pid: 2,
                process_generation: 3,
                producer_id: 4,
                thread_generation: 5,
                os_tid: 6,
                sequence: 7,
                engine_id: Some(8),
            },
            kind: DecodedKind::SyscallEnter(SyscallEnter {
                semantics: SyscallSemantics::Raw,
                nr: 9,
                args: [10, 11, 12, 13, 14, 15],
            }),
        }
    }

    #[test]
    fn filter_parser_is_strict_and_end_exclusive() {
        let filter = Filter::parse(&[
            "--engine".into(),
            "8".into(),
            "--producer".into(),
            "4".into(),
            "--sequence".into(),
            "7:8".into(),
        ])
        .unwrap();
        assert!(filter.matches(&event()));
        assert!(Filter::parse(&["--sequence".into(), "8:8".into()]).is_err());
        assert!(Filter::parse(&["--kind".into(), "made-up".into()]).is_err());
    }

    #[test]
    fn json_uses_strings_for_ids_and_hex_for_raw_arguments() {
        let value = event_json(&event());
        assert_eq!(value["producer_id"], "4");
        assert_eq!(value["sequence"], "7");
        assert_eq!(value["args"][0], "0x000000000000000a");
        assert_eq!(value["timestamp"], Value::Null);
        assert_eq!(value["time_status"], "unavailable");
    }
}
