use crate::api::Api;
use crate::host::{self, Host, HostEvent, Paths};
use crate::model::ServerEvent;
use crate::secrets;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use iced::futures::channel::mpsc as fmpsc;
use iced::futures::StreamExt;
use iced::task::{Never, Sipper, sipper};
use rand::RngCore;

#[derive(Debug, Clone)]
pub enum Event {
    Status {
        message: String,
        progress: Option<f64>,
    },
    Connected(Connection),
    RuntimeReady,
    Server(ServerEvent),
    LiveLost(String),
    Working,
    Failed(String),
    Stopped,
}

#[derive(Debug, Clone)]
pub struct Connection {
    pub api: Api,
    commands: fmpsc::Sender<Command>,
}

impl Connection {
    pub fn quit(&mut self) {
        let _ = self.commands.try_send(Command::Quit);
    }
}

#[derive(Debug)]
enum Command {
    Quit,
}

fn keep_awake(on: bool) {
    use windows_sys::Win32::System::Power::{
        ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };
    unsafe {
        SetThreadExecutionState(if on {
            ES_CONTINUOUS | ES_SYSTEM_REQUIRED
        } else {
            ES_CONTINUOUS
        });
    }
}

pub fn run() -> impl Sipper<Never, Event> {
    sipper(async |mut output| {
        let prepared = (|| -> Result<_, String> {
            let paths = Paths::discover()?;
            if host::original_app_running(&paths.install) {
                return Err("The original Nod app is still running. Quit it from its icon next to the clock, then open this app again.".into());
            }
            let vault = secrets::vault_key(&paths.user_data, &paths.profile).map_err(|e| e.to_string())?;
            let mut token = [0u8; 32];
            rand::rng().fill_bytes(&mut token);
            let token = URL_SAFE_NO_PAD.encode(token);
            let port = host::free_port()?;
            Ok((paths, vault, token, port))
        })();
        let (paths, vault, token, port) = match prepared {
            Ok(value) => value,
            Err(message) => {
                output.send(Event::Failed(message)).await;
                return std::future::pending().await;
            }
        };
        let (mut host, mut host_events) = match Host::spawn(&paths, &token, &vault, port) {
            Ok(value) => value,
            Err(message) => {
                output.send(Event::Failed(message)).await;
                return std::future::pending().await;
            }
        };
        drop(vault);
        let (commands, mut command_rx) = fmpsc::channel::<Command>(4);
        let mut live: Option<tokio::sync::mpsc::UnboundedReceiver<Result<ServerEvent, String>>> =
            None;
        loop {
            tokio::select! {
                event = host_events.recv() => {
                    let Some(event) = event else {
                        output.send(Event::Failed("Nod's background service stopped unexpectedly.".into())).await;
                        keep_awake(false);
                        return std::future::pending().await;
                    };
                    match event {
                        HostEvent::Status { message, progress, .. } => {
                            if let Some(message) = message {
                                output.send(Event::Status { message, progress }).await;
                            }
                        }
                        HostEvent::EngineReady { origin } => {
                            let api = Api::new(&origin, &token);
                            live = Some(api.events());
                            output.send(Event::Connected(Connection { api, commands: commands.clone() })).await;
                        }
                        HostEvent::Ready => output.send(Event::RuntimeReady).await,
                        HostEvent::Working { working } => {
                            keep_awake(working);
                            output.send(Event::Working).await;
                        }
                        HostEvent::EngineExit { code, message } => {
                            output.send(Event::Failed(format!(
                                "Nod's background service stopped{}.{}",
                                code.map(|c| format!(" (code {c})")).unwrap_or_default(),
                                if message.is_empty() { String::new() } else { format!(" {message}") }
                            ))).await;
                        }
                        HostEvent::Error { message, .. } => output.send(Event::Failed(message)).await,
                        HostEvent::Stopped => {
                            keep_awake(false);
                            output.send(Event::Stopped).await;
                            return std::future::pending().await;
                        }
                    }
                }
                event = async {
                    match live.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match event {
                        Some(Ok(event)) => output.send(Event::Server(event)).await,
                        Some(Err(message)) => output.send(Event::LiveLost(message)).await,
                        None => live = None,
                    }
                }
                command = command_rx.next() => {
                    if let Some(Command::Quit) = command {
                        output.send(Event::Status { message: "Closing Nod...".into(), progress: None }).await;
                        host.quit().await;
                        keep_awake(false);
                        output.send(Event::Stopped).await;
                        return std::future::pending().await;
                    }
                }
            }
        }
    })
}
