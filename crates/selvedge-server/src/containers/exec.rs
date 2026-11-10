//! `exec()` in a container instance.
//!
//! workerd runs a command in a container through Docker's exec API: create,
//! then start on a hijacked connection that carries stdin one way and
//! stdout and stderr the other, then inspect for the exit code. The agent
//! keeps that API towards workerd. For each started exec it opens an
//! upgraded connection to the controller, which runs the command through
//! its backend: Docker's own exec, or the pod's `exec` subresource on
//! Kubernetes. A runner never reaches an instance's runtime itself.
//!
//! The upgraded connection carries frames in Docker's multiplexed-stream
//! layout: a type byte, three zero bytes, a big-endian length and the
//! payload. Types 1 and 2 are stdout and stderr, as in Docker, so the agent
//! can hand them to workerd as they are; the others are Selvedge's.

use anyhow::{Result, bail};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// From the agent: bytes for the command's stdin.
pub const STDIN: u8 = 0;
/// From the controller: the command's output.
pub const STDOUT: u8 = 1;
pub const STDERR: u8 = 2;
/// From the agent: stdin is done (no payload).
pub const CLOSE_STDIN: u8 = 3;
/// From the agent: the terminal's new size, as JSON `{"cols", "rows"}`.
pub const RESIZE: u8 = 4;
/// From the controller, last: [`Exit`] as JSON.
pub const EXIT: u8 = 5;

/// The largest frame either side accepts.
const MAX_FRAME: usize = 1 << 20;

/// The header that carries an exec's [`ExecSpec`] to the controller.
pub const SPEC_HEADER: &str = "x-selvedge-exec";
/// The protocol name of the upgraded connection.
pub const PROTOCOL: &str = "selvedge-exec";

/// What to run, as workerd asked for it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExecSpec {
    pub cmd: Vec<String>,
    /// `KEY=value`.
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub tty: bool,
    #[serde(default)]
    pub stdin: bool,
    #[serde(default)]
    pub stdout: bool,
    #[serde(default)]
    pub stderr: bool,
    /// The terminal's initial size, with `tty`; 0 for the default.
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub rows: u16,
}

/// How an exec ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Exit {
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum Input {
    Stdin(Bytes),
    CloseStdin,
    Resize { cols: u16, rows: u16 },
}

#[derive(Debug, PartialEq)]
pub enum Output {
    Stdout(Bytes),
    Stderr(Bytes),
}

/// A backend's side of a running exec: what to send to the command, and
/// where its output goes. The backend drops `output` when the command's
/// streams end.
pub struct ExecIo {
    pub input: mpsc::Receiver<Input>,
    pub output: mpsc::Sender<Output>,
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, kind: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut header = [kind, 0, 0, 0, 0, 0, 0, 0];
    header[4..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    w.write_all(&header).await?;
    w.write_all(payload).await?;
    w.flush().await
}

/// The next frame, or `None` at the end of the stream.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<(u8, Bytes)>> {
    let mut header = [0u8; 8];
    match r.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if len > MAX_FRAME {
        bail!("an exec frame of {len} bytes");
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    Ok(Some((header[0], payload.into())))
}

/// An input frame as the controller understands it.
pub fn input_of(kind: u8, payload: Bytes) -> Option<Input> {
    match kind {
        STDIN => Some(Input::Stdin(payload)),
        CLOSE_STDIN => Some(Input::CloseStdin),
        RESIZE => {
            #[derive(Deserialize)]
            struct Size {
                cols: u16,
                rows: u16,
            }
            let s: Size = serde_json::from_slice(&payload).ok()?;
            Some(Input::Resize { cols: s.cols, rows: s.rows })
        }
        _ => None,
    }
}

/// Splits Docker's multiplexed exec stream into output, for a non-TTY exec.
/// Answers when the stream ends or `output` is closed.
pub async fn demux<R: AsyncRead + Unpin>(r: &mut R, output: &mpsc::Sender<Output>) -> Result<()> {
    while let Some((kind, payload)) = read_frame(r).await? {
        if payload.is_empty() {
            continue;
        }
        let o = match kind {
            STDERR => Output::Stderr(payload),
            _ => Output::Stdout(payload),
        };
        if output.send(o).await.is_err() {
            break;
        }
    }
    Ok(())
}

/// Copies a raw stream (a TTY exec's) into output as stdout.
pub async fn copy_raw<R: AsyncRead + Unpin>(r: &mut R, output: &mpsc::Sender<Output>) -> Result<()> {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 || output.send(Output::Stdout(Bytes::copy_from_slice(&buf[..n]))).await.is_err() {
            return Ok(());
        }
    }
}

/// A shell word: `'...'` with single quotes escaped.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The command as a runtime without exec options runs it: environment and
/// working directory applied by `sh` first. The user is not supported that
/// way; the caller refuses it.
pub fn wrapped_command(spec: &ExecSpec) -> Vec<String> {
    if spec.env.is_empty() && spec.working_dir.as_deref().is_none_or(str::is_empty) {
        return spec.cmd.clone();
    }
    let mut script = String::new();
    if let Some(dir) = spec.working_dir.as_deref().filter(|d| !d.is_empty()) {
        script.push_str(&format!("cd {} || exit 126; ", quote(dir)));
    }
    for e in &spec.env {
        if let Some((k, v)) = e.split_once('=') {
            if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                script.push_str(&format!("export {k}={}; ", quote(v)));
            }
        }
    }
    script.push_str("exec \"$@\"");
    let mut cmd = vec!["/bin/sh".to_owned(), "-c".to_owned(), script, "sh".to_owned()];
    cmd.extend(spec.cmd.iter().cloned());
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, STDOUT, b"hello").await.unwrap();
        write_frame(&mut buf, EXIT, br#"{"exit_code":3}"#).await.unwrap();
        assert_eq!(&buf[..8], &[1, 0, 0, 0, 0, 0, 0, 5]);
        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r).await.unwrap(), Some((STDOUT, Bytes::from_static(b"hello"))));
        let (kind, payload) = read_frame(&mut r).await.unwrap().unwrap();
        assert_eq!(kind, EXIT);
        let exit: Exit = serde_json::from_slice(&payload).unwrap();
        assert_eq!(exit.exit_code, Some(3));
        assert_eq!(read_frame(&mut r).await.unwrap(), None);
    }

    #[tokio::test]
    async fn oversized_frames_are_refused() {
        let header = [1u8, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];
        assert!(read_frame(&mut &header[..]).await.is_err());
    }

    #[test]
    fn inputs() {
        assert_eq!(input_of(STDIN, Bytes::from_static(b"x")), Some(Input::Stdin(Bytes::from_static(b"x"))));
        assert_eq!(input_of(CLOSE_STDIN, Bytes::new()), Some(Input::CloseStdin));
        assert_eq!(
            input_of(RESIZE, Bytes::from_static(br#"{"cols":80,"rows":24}"#)),
            Some(Input::Resize { cols: 80, rows: 24 })
        );
        assert_eq!(input_of(EXIT, Bytes::new()), None);
    }

    #[test]
    fn wrapping() {
        let plain = ExecSpec { cmd: vec!["ls".into()], ..Default::default() };
        assert_eq!(wrapped_command(&plain), vec!["ls"]);
        let spec = ExecSpec {
            cmd: vec!["echo".into(), "a b".into()],
            env: vec!["FOO=it's".into(), "bad name=x".into()],
            working_dir: Some("/tmp".into()),
            ..Default::default()
        };
        assert_eq!(
            wrapped_command(&spec),
            vec![
                "/bin/sh",
                "-c",
                r#"cd '/tmp' || exit 126; export FOO='it'\''s'; exec "$@""#,
                "sh",
                "echo",
                "a b"
            ]
        );
    }
}
