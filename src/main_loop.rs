use crate::config::DBusType;
#[cfg(feature = "dbus_mpris")]
use crate::dbus_mpris::DbusServer;
use crate::process::spawn_program_on_event;
use futures::{
    self,
    future::{self, Fuse, FusedFuture},
    stream::Peekable,
    Future, FutureExt, StreamExt,
};
use librespot_connect::spirc::Spirc;
use librespot_connect::config::ConnectConfig;
use librespot_core::{
    authentication::Credentials,
    cache::Cache,
    config::{DeviceType, SessionConfig},
    session::Session,
};
use librespot_discovery::Discovery;
use librespot_playback::{
    audio_backend::Sink,
    config::{AudioFormat, PlayerConfig},
    mixer::Mixer,
    player::Player,
};
use log::error;
use std::pin::Pin;
use std::sync::Arc;

pub struct AudioSetup {
    pub mixer: Box<dyn FnMut() -> Arc<dyn Mixer>>,
    pub backend: fn(Option<String>, AudioFormat) -> Box<dyn Sink>,
    pub audio_device: Option<String>,
    pub audio_format: AudioFormat,
}

pub struct SpotifydState {
    pub cache: Option<Cache>,
    pub device_name: String,
    pub player_event_program: Option<String>,
}

pub(crate) enum CredentialsProvider {
    Discovery(Peekable<Discovery>),
    SpotifyCredentials(Credentials),
}

impl From<Discovery> for CredentialsProvider {
    fn from(stream: Discovery) -> Self {
        CredentialsProvider::Discovery(stream.peekable())
    }
}

impl CredentialsProvider {
    async fn get_credentials(&mut self) -> Credentials {
        match self {
            CredentialsProvider::Discovery(stream) => stream.next().await.unwrap(),
            CredentialsProvider::SpotifyCredentials(creds) => creds.clone(),
        }
    }

    // wait for an incoming connection if the underlying provider is a discovery stream
    async fn incoming_connection(&mut self) {
        match self {
            CredentialsProvider::Discovery(stream) => {
                let peeked = Pin::new(stream).peek().await;
                if peeked.is_none() {
                    future::pending().await
                }
            }
            _ => future::pending().await,
        }
    }
}

pub(crate) struct MainLoop {
    pub(crate) audio_setup: AudioSetup,
    pub(crate) spotifyd_state: SpotifydState,
    pub(crate) player_config: PlayerConfig,
    pub(crate) session_config: SessionConfig,
    pub(crate) has_volume_ctrl: bool,
    pub(crate) initial_volume: Option<u16>,
    pub(crate) shell: String,
    pub(crate) device_type: DeviceType,
    #[cfg_attr(not(feature = "dbus_mpris"), allow(unused))]
    pub(crate) use_mpris: bool,
    #[cfg_attr(not(feature = "dbus_mpris"), allow(unused))]
    pub(crate) dbus_type: DBusType,
    pub(crate) credentials_provider: CredentialsProvider,
}

impl MainLoop {
    async fn get_session(&mut self) -> Result<Session, librespot_core::Error> {
        let creds = self.credentials_provider.get_credentials().await;

        let session_config = self.session_config.clone();
        let cache = self.spotifyd_state.cache.clone();
        let session = Session::new(session_config, cache);

        session.connect(creds, true)
            .await?;
        Ok(session)
    }

    pub(crate) async fn run(&mut self) {
        tokio::pin! {
            let ctrl_c = tokio::signal::ctrl_c();
        }

        'mainloop: loop {
            let session = tokio::select!(
                _ = &mut ctrl_c => {
                    break 'mainloop;
                }
                session = self.get_session() => {
                    match session {
                        Ok(session) => session,
                        Err(err) => {
                            error!("failed to connect to spotify: {}", err);
                            break 'mainloop;
                        }
                    }
                }
            );
            let creds = self.credentials_provider.get_credentials().await;

            let mixer = (self.audio_setup.mixer)();
            let backend = self.audio_setup.backend;
            let audio_device = self.audio_setup.audio_device.clone();
            let audio_format = self.audio_setup.audio_format;
            let player = Player::new(
                self.player_config.clone(),
                session.clone(),
                mixer.get_soft_volume(),
                move || (backend)(audio_device, audio_format),
            );
            let mut event_channel = player.get_player_event_channel();

            let (spirc, spirc_task) = match Spirc::new(
                ConnectConfig {
                    name: self.spotifyd_state.device_name.clone(),
                    device_type: self.device_type,
                    is_group: false, // TODO?
                    initial_volume: self.initial_volume,
                    has_volume_ctrl: self.has_volume_ctrl,
                },
                session.clone(),
                creds,
                player,
                mixer,
            )
            .await
            {
                Ok((spirc, task)) => (spirc, task),
                Err(err) => {
                    error!("failed to create spirc: {}", err);
                    break 'mainloop;
                }
            };


            tokio::pin!(spirc_task);

            let shared_spirc = Arc::new(spirc);

            // we don't necessarily have a dbus server
            let mut dbus_server: Pin<Box<dyn Future<Output = ()>>> = Box::pin(future::pending());

            #[cfg(feature = "dbus_mpris")]
            let mpris_event_tx = if self.use_mpris {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                dbus_server = Box::pin(DbusServer::new(
                    session,
                    shared_spirc.clone(),
                    self.spotifyd_state.device_name.clone(),
                    rx,
                    self.dbus_type,
                ));
                Some(tx)
            } else {
                None
            };

            let mut running_event_program = Box::pin(Fuse::terminated());

            loop {
                tokio::select!(
                    // a new session has been started via the discovery stream
                    _ = self.credentials_provider.incoming_connection() => {
                        if let Err(e) = shared_spirc.shutdown() {
                            error!("failed to shutdown spirc: {}", e);
                        }
                        break;
                    }
                    // the program should shut down
                    _ = &mut ctrl_c => {
                        if let Err(e) = shared_spirc.shutdown() {
                            error!("failed to shutdown spirc: {}", e);
                        }
                        break 'mainloop;
                    }
                    // spirc was shut down by some external factor
                    _ = &mut spirc_task => {
                        break;
                    }
                    // dbus stopped unexpectedly
                    _ = &mut dbus_server => {
                        if let Err(e) = shared_spirc.shutdown() {
                            error!("failed to shutdown spirc: {}", e);
                        }
                        break 'mainloop;
                    }
                    // a new player event is available and no program is running
                    event = event_channel.recv(), if running_event_program.is_terminated() => {
                        let event = event.unwrap();
                        #[cfg(feature = "dbus_mpris")]
                        if let Some(ref tx) = mpris_event_tx {
                            tx.send(event.clone()).unwrap();
                        }
                        if let Some(ref cmd) = self.spotifyd_state.player_event_program {
                            match spawn_program_on_event(&self.shell, cmd, event) {
                                Ok(child) => running_event_program = Box::pin(child.wait().fuse()),
                                Err(e) => error!("{}", e),
                            }
                        }
                    }
                    // a running program has finished
                    result = &mut running_event_program, if !running_event_program.is_terminated() => {
                        match result {
                            // Exited without error...
                            Ok(_) => (),
                            // Exited with error...
                            Err(e) => error!("{}", e),
                        }
                    }
                )
            }
        }
    }
}
