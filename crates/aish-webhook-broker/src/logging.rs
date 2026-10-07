//! Broker log output format (TASK-375).
//!
//! `LOG_FORMAT=text` (default) keeps the human-readable `tracing` output.
//! `LOG_FORMAT=json` emits one JSON object per line — what the SigNoz / OTel
//! collector log pipelines (filelog, docker, fly log shipper) parse natively.

use std::str::FromStr;

use tracing::Subscriber;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

/// Log line format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    /// Human-readable text (default).
    #[default]
    Text,
    /// One JSON object per line.
    Json,
}

impl FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(format!(
                "invalid log format {other:?} (expected \"text\" or \"json\")"
            )),
        }
    }
}

/// Build the broker's tracing subscriber for `format`, writing to `writer`.
pub fn subscriber<W>(
    format: LogFormat,
    filter: EnvFilter,
    writer: W,
) -> Box<dyn Subscriber + Send + Sync>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_writer(writer);
    match format {
        LogFormat::Text => Box::new(builder.finish()),
        LogFormat::Json => Box::new(builder.json().with_current_span(false).finish()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn emit(format: LogFormat) -> String {
        let buf = Buf::default();
        let sub = subscriber(format, EnvFilter::new("info"), buf.clone());
        tracing::subscriber::with_default(sub, || {
            tracing::info!(tenant = "acme", plugin = "github", "webhook received");
        });
        let out = buf.0.lock().unwrap().clone();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn parses_formats() {
        assert_eq!("text".parse::<LogFormat>(), Ok(LogFormat::Text));
        assert_eq!("json".parse::<LogFormat>(), Ok(LogFormat::Json));
        assert_eq!(" JSON ".parse::<LogFormat>(), Ok(LogFormat::Json));
        assert!("xml".parse::<LogFormat>().is_err());
        assert_eq!(LogFormat::default(), LogFormat::Text);
    }

    #[test]
    fn json_format_emits_one_json_object_per_line() {
        let out = emit(LogFormat::Json);
        let line = out.lines().next().expect("one log line");
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        assert_eq!(v["level"], "INFO");
        assert_eq!(v["fields"]["message"], "webhook received");
        assert_eq!(v["fields"]["tenant"], "acme");
    }

    #[test]
    fn text_format_is_not_json() {
        let out = emit(LogFormat::Text);
        assert!(out.contains("webhook received"));
        assert!(serde_json::from_str::<serde_json::Value>(out.trim()).is_err());
    }
}
