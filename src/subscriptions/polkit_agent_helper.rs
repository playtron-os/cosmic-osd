use cosmic::iced::{self, Subscription};
use futures::SinkExt;
use std::ffi::OsString;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::{fmt, io};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::process::Command;
use tokio::select;
use tokio::sync::mpsc::{self, Sender};

const HELPER_BIN_PATH: Option<&str> = option_env!("POLKIT_AGENT_HELPER_1");
const HELPER_SOCKET_PATH: &str = "/run/polkit/agent-helper.socket";

// polkit-agent-helper-1 lives in different places per distro, and on Nix-style
// systems in a store path nobody can know at build time. Probe known locations
// (Fedora/Debian use /usr/lib/polkit-1) so we don't ENOENT on a single
// hardcoded path.
const HELPER_BIN_CANDIDATES: &[&str] = &[
    "/usr/lib/polkit-1/polkit-agent-helper-1",
    "/usr/libexec/polkit-1/polkit-agent-helper-1",
    "/usr/libexec/polkit-agent-helper-1",
    // Nix-style systems: the setuid wrapper, then the system profile. The store
    // copy cannot carry the setuid bit, so the wrapper has to be preferred.
    "/run/wrappers/bin/polkit-agent-helper-1",
    "/run/current-system/sw/lib/polkit-1/polkit-agent-helper-1",
];

/// Locate the `polkit-agent-helper-1` binary that runs the PAM conversation.
///
/// The runtime environment wins over the compile-time override: a package whose
/// helper lives outside the FHS can point `POLKIT_AGENT_HELPER_1` at it without
/// patching or rebuilding us. With the variable unset — the normal case on
/// Fedora — this resolves exactly as before.
fn resolve_helper_bin_path() -> Option<PathBuf> {
    resolve_helper_bin_path_in(
        std::env::var_os("POLKIT_AGENT_HELPER_1"),
        HELPER_BIN_PATH,
        HELPER_BIN_CANDIDATES,
        &|path| path.exists(),
    )
}

fn resolve_helper_bin_path_in(
    runtime_override: Option<OsString>,
    compiled_override: Option<&str>,
    candidates: &[&str],
    exists: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(path) = runtime_override.filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }

    if let Some(path) = compiled_override.filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }

    candidates
        .iter()
        .map(Path::new)
        .find(|path| exists(path))
        .map(Path::to_path_buf)
}

#[derive(Clone, Debug)]
pub enum Event {
    Failed,
    Responder(Responder),
    Request(String, bool),
    ShowError(String),
    ShowDebug(String),
    Complete(bool),
}

pub fn subscription(pw_name: &str, cookie: &str) -> Subscription<Event> {
    let args = (pw_name.to_owned(), cookie.to_owned());
    Subscription::run_with(args, |args| {
        let pw_name = args.0.to_owned();
        let cookie = args.1.to_owned();

        iced::stream::channel(16, async move |mut output| {
            for _ in 0..3 {
                let ControlFlow::Break(successful) =
                    try_authenticate(&pw_name, &cookie, &mut output).await
                else {
                    continue;
                };

                if successful {
                    log::debug!("authenticated successfully");
                    return;
                };

                log::debug!("retrying authentication");
            }

            log::info!("retries exhausted");

            let _ = output.send(Event::Failed).await;
        })
    })
}

#[derive(Clone)]
pub struct Responder {
    sender: Sender<String>,
}

impl Responder {
    pub async fn response(&self, resp: &str) -> Result<(), ()> {
        self.sender.send(resp.to_owned()).await.map_err(|_| ())?;

        Ok(())
    }
}

impl fmt::Debug for Responder {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Responder")
    }
}

async fn try_authenticate(
    pw_name: &str,
    cookie: &str,
    output: &mut futures::channel::mpsc::Sender<Event>,
) -> ControlFlow<bool> {
    let mut agent_helper = match AgentHelper::new(pw_name, cookie).await {
        Ok(agent_helper) => agent_helper,
        Err(err) => {
            log::error!("failed to create helper, {}", err.kind());
            let _ = output.send(Event::Failed).await;

            return ControlFlow::Break(false);
        }
    };

    let (sender, mut receiver) = mpsc::channel::<String>(16);
    let _ = output.send(Event::Responder(Responder { sender })).await;

    loop {
        select! {
            next = agent_helper.next() => agent_next(next, output).await?,
            Some(msg) = receiver.recv() =>
                responder_next(
                    &msg,
                    &mut agent_helper,
                    output
                )
                    .await
                    .map_break(|_| false)?,
        }
    }
}

async fn agent_next(
    next: io::Result<Option<Event>>,
    output: &mut futures::channel::mpsc::Sender<Event>,
) -> ControlFlow<bool> {
    match next {
        Ok(Some(event)) => {
            let Event::Complete(successful) = event else {
                let _ = output.send(event).await;
                return ControlFlow::Continue(());
            };

            log::debug!("got completed event (successful: {successful}), exiting");
            let _ = output.send(event).await;

            ControlFlow::Break(successful)
        }
        Ok(None) => {
            log::debug!("no next message from helper, exiting");
            ControlFlow::Break(false)
        }
        Err(err) => {
            log::error!("failed to get next message from helper: {}", err.kind());
            let _ = output.send(Event::Failed).await;
            ControlFlow::Break(false)
        }
    }
}

async fn responder_next(
    msg: &str,
    agent_helper: &mut AgentHelper,
    output: &mut futures::channel::mpsc::Sender<Event>,
) -> ControlFlow<()> {
    if let Err(err) = agent_helper.write(msg).await {
        log::error!(
            "failed to send message from the responder to the auth helper, error: {}",
            err.kind()
        );
        let _ = output.send(Event::Failed).await;

        ControlFlow::Break(())
    } else {
        ControlFlow::Continue(())
    }
}

enum AgentHelper {
    Bin {
        _child: Box<tokio::process::Child>,
        stdout: BufReader<tokio::process::ChildStdout>,
        stdin: BufWriter<tokio::process::ChildStdin>,
    },
    Socket {
        read_half: BufReader<OwnedReadHalf>,
        write_half: OwnedWriteHalf,
    },
}

impl AgentHelper {
    async fn new(pw_name: &str, cookie: &str) -> io::Result<Self> {
        let mut agent_helper = if Path::new(HELPER_SOCKET_PATH).exists() {
            Self::new_socket(pw_name).await?
        } else {
            Self::new_bin(pw_name).await?
        };

        agent_helper.write(cookie).await?;

        Ok(agent_helper)
    }

    async fn new_socket(pw_name: &str) -> io::Result<Self> {
        log::info!("using socket");

        let stream = UnixStream::connect(HELPER_SOCKET_PATH).await?;
        let (read, write_half) = stream.into_split();

        let read_half = BufReader::new(read);

        let mut agent_helper = Self::Socket {
            read_half,
            write_half,
        };

        agent_helper.write(pw_name).await?;

        Ok(agent_helper)
    }

    async fn new_bin(pw_name: &str) -> io::Result<Self> {
        log::info!("using binary");

        let Some(helper_bin_path) = resolve_helper_bin_path() else {
            log::error!(
                "no polkit-agent-helper-1 found; set POLKIT_AGENT_HELPER_1 or install it at one of {HELPER_BIN_CANDIDATES:?}"
            );

            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "polkit-agent-helper-1 not found",
            ));
        };
        log::trace!("using helper binary from: {}", helper_bin_path.display());

        let mut child = Command::new(helper_bin_path)
            .kill_on_drop(true)
            .arg(pw_name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;

        let stdin = BufWriter::new(child.stdin.take().unwrap());
        let stdout = BufReader::new(child.stdout.take().unwrap());

        Ok(Self::Bin {
            _child: Box::new(child),
            stdin,
            stdout,
        })
    }

    async fn next(&mut self) -> io::Result<Option<Event>> {
        let reader: &mut (dyn Unpin + Send + Sync + AsyncBufRead) = match self {
            Self::Bin { stdout, .. } => stdout,
            Self::Socket { read_half, .. } => read_half,
        };

        let mut line = String::new();
        while reader.read_line(&mut line).await? != 0 {
            match event(&line) {
                Ok(event) => return Ok(Some(event)),
                Err(prefix) => {
                    log::error!(
                        "Unknown prefix: '{prefix}' in line '{line}' from 'polkit-agent-helper-1'"
                    );
                    continue;
                }
            }
        }

        Ok(None)
    }

    async fn write(&mut self, msg: &str) -> io::Result<()> {
        let msg = format!("{msg}\n");

        let writer: &mut (dyn Unpin + Send + Sync + AsyncWrite) = match self {
            Self::Bin { stdin, .. } => stdin,
            Self::Socket { write_half, .. } => write_half,
        };

        writer.write_all(msg.as_bytes()).await?;
        writer.flush().await?;

        Ok(())
    }
}

fn event(line: &str) -> Result<Event, &str> {
    let line = line.trim();
    let (prefix, rest) = line.split_once(' ').unwrap_or((line, ""));

    Ok(match prefix {
        "PAM_PROMPT_ECHO_OFF" => Event::Request(rest.to_string(), false),
        "PAM_PROMPT_ECHO_ON" => Event::Request(rest.to_string(), true),
        "PAM_ERROR_MSG" => Event::ShowError(rest.to_string()),
        "PAM_TEXT_INFO" => Event::ShowDebug(rest.to_string()),
        "SUCCESS" => Event::Complete(true),
        "FAILURE" => Event::Complete(false),
        unknown_prefix => {
            return Err(unknown_prefix);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEDORA_HELPER: &str = "/usr/lib/polkit-1/polkit-agent-helper-1";
    const NIX_HELPER: &str = "/run/wrappers/bin/polkit-agent-helper-1";

    fn present(paths: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        move |path| paths.iter().any(|present| Path::new(present) == path)
    }

    #[test]
    fn compile_time_override_is_used_when_the_environment_is_silent() {
        // How a Fedora build behaves today: `just build-release` bakes the path in.
        assert_eq!(
            resolve_helper_bin_path_in(
                None,
                Some(FEDORA_HELPER),
                HELPER_BIN_CANDIDATES,
                &present(&[])
            ),
            Some(PathBuf::from(FEDORA_HELPER))
        );
    }

    #[test]
    fn runtime_override_wins_over_the_compile_time_one() {
        let store_helper = "/nix/store/00000000-polkit/lib/polkit-1/polkit-agent-helper-1";

        assert_eq!(
            resolve_helper_bin_path_in(
                Some(OsString::from(store_helper)),
                Some(FEDORA_HELPER),
                HELPER_BIN_CANDIDATES,
                &present(&[FEDORA_HELPER])
            ),
            Some(PathBuf::from(store_helper))
        );
    }

    #[test]
    fn empty_overrides_are_ignored() {
        assert_eq!(
            resolve_helper_bin_path_in(
                Some(OsString::new()),
                Some(""),
                HELPER_BIN_CANDIDATES,
                &present(&[FEDORA_HELPER])
            ),
            Some(PathBuf::from(FEDORA_HELPER))
        );
    }

    #[test]
    fn fhs_layout_is_probed() {
        assert_eq!(
            resolve_helper_bin_path_in(
                None,
                None,
                HELPER_BIN_CANDIDATES,
                &present(&[FEDORA_HELPER])
            ),
            Some(PathBuf::from(FEDORA_HELPER))
        );
    }

    #[test]
    fn nix_layout_is_probed() {
        // Nothing under /usr exists there; the setuid wrapper does.
        assert_eq!(
            resolve_helper_bin_path_in(None, None, HELPER_BIN_CANDIDATES, &present(&[NIX_HELPER])),
            Some(PathBuf::from(NIX_HELPER))
        );
    }

    #[test]
    fn nothing_found_is_reported_rather_than_spawned() {
        assert_eq!(
            resolve_helper_bin_path_in(None, None, HELPER_BIN_CANDIDATES, &present(&[])),
            None
        );
    }
}
