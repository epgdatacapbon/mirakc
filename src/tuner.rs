use std::collections::HashMap;
use std::fmt;
use std::process::{Child, Stdio};
use std::sync::Arc;

use actix::prelude::*;
use cfg_if;
use log;
use mustache;

use crate::broadcaster::*;
use crate::command_util;
use crate::config::{Config, TunerConfig};
use crate::error::Error;
use crate::models::*;
use crate::mpeg_ts_stream::MpegTsStream;
use crate::tokio_snippet;

pub fn start(config: Arc<Config>) {
    let addr = TunerManager::new(config).start();
    actix::registry::SystemRegistry::set(addr);
}

pub async fn query_tuners() -> Result<Vec<MirakurunTuner>, Error> {
    cfg_if::cfg_if! {
        if #[cfg(test)] {
            Ok(Vec::new())
        } else {
            TunerManager::from_registry().send(QueryTunersMessage).await?
        }
    }
}

pub async fn start_streaming(
    channel_type: ChannelType,
    channel: String,
    user: TunerUser
)-> Result<MpegTsStream, Error> {
    cfg_if::cfg_if! {
        if #[cfg(test)] {
            let _ = (channel_type, channel, user);
            let (_, receiver) = tokio::sync::mpsc::channel(1);
            Ok(MpegTsStream::new(Default::default(), receiver))
        } else {
            TunerManager::from_registry().send(StartStreamingMessage {
                channel_type, channel, user
            }).await?
        }
    }
}

pub fn stop_streaming(id: TunerSubscriptionId) {
    cfg_if::cfg_if! {
        if #[cfg(test)] {
            let _ = id;
        } else {
            TunerManager::from_registry().do_send(StopStreamingMessage {
                id
            });
        }
    }
}

// identifiers

#[derive(Clone, Copy, Default, PartialEq)]
pub struct TunerSessionId {
    tuner_index: usize,
    tuner_pid: u32,
}

impl fmt::Display for TunerSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tuner#{}.{}", self.tuner_index, self.tuner_pid)
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
pub struct TunerSubscriptionId {
    session_id: TunerSessionId,
    serial_number: u32,
}

impl fmt::Display for TunerSubscriptionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tuner#{}.{}.{}",
               self.session_id.tuner_index, self.session_id.tuner_pid,
               self.serial_number)
    }
}

// tuner manager

struct TunerManager {
    config: Arc<Config>,
    tuners: Vec<Tuner>,
}

struct TunerSubscription {
    id: TunerSubscriptionId,
    broadcaster: Addr<Broadcaster>,
}

impl TunerManager {
    fn new(config: Arc<Config>) -> Self {
        TunerManager { config, tuners: Vec::new() }
    }

    fn load_tuners(&mut self) {
        log::info!("Loading tuners...");
        let tuners: Vec<Tuner> = self.config
            .tuners
            .iter()
            .filter(|config| !config.disabled)
            .enumerate()
            .map(|(i, config)| Tuner::new(i, config))
            .collect();
        log::info!("Loaded {} tuners", tuners.len());
        self.tuners = tuners;
    }

    fn activate_tuner(
        &mut self,
        channel_type: ChannelType,
        channel: String,
        user: TunerUser,
    ) -> Result<TunerSubscription, Error> {
        if let TunerUserInfo::Tracker { stream_id } = user.info {
            let tuner = &mut self.tuners[stream_id.session_id.tuner_index];
            if tuner.is_active() {
                return Ok(tuner.subscribe(user));
            }
            return Err(Error::TunerUnavailable);
        }

        let found = self.tuners
            .iter_mut()
            .find(|tuner| tuner.is_reuseable(channel_type, &channel));
        if let Some(tuner) = found {
            log::info!("tuner#{}: Reuse tuner already activated with {} {}",
                       tuner.index, channel_type, channel);
            return Ok(tuner.subscribe(user));
        }

        let found = self.tuners
            .iter_mut()
            .find(|tuner| tuner.is_available_for(channel_type));
        if let Some(tuner) = found {
            log::info!("tuner#{}: Activate with {} {}",
                       tuner.index, channel_type, channel);
            tuner.activate(channel_type, channel)?;
            return Ok(tuner.subscribe(user));
        }

        // No available tuner at this point.  Take over the right to use
        // a tuner used by a low priority user.
        let found = self.tuners
            .iter_mut()
            .filter(|tuner| tuner.is_supported_type(channel_type))
            .find(|tuner| tuner.can_grab(user.priority));
        if let Some(tuner) = found {
            log::info!("tuner#{}: Grab tuner, rectivate with {} {}",
                       tuner.index, channel_type, channel);
            tuner.deactivate();
            tuner.activate(channel_type, channel)?;
            return Ok(tuner.subscribe(user));
        }

        log::warn!("No tuner available for {} {} {}",
                   channel_type, channel, user);
        Err(Error::TunerUnavailable)
    }

    fn deactivate_tuner(&mut self, id: TunerSubscriptionId) {
        log::info!("tuner#{}: Deactivate", id.session_id.tuner_index);
        self.tuners[id.session_id.tuner_index].deactivate();
    }

    fn stop_streaming(&mut self, id: TunerSubscriptionId) {
        log::info!("{}: Stop streaming", id);
        let _ = self.tuners[id.session_id.tuner_index].stop_streaming(id);
    }
}

impl Actor for TunerManager {
    type Context = actix::Context<Self>;

    fn started(&mut self, _: &mut Self::Context) {
        log::debug!("Started");
        self.load_tuners();
    }

    fn stopped(&mut self, _: &mut Self::Context) {
        for tuner in self.tuners.iter_mut() {
            tuner.deactivate();
        }
        log::debug!("Stopped");
    }
}

impl Supervised for TunerManager {}
impl SystemService for TunerManager {}

impl Default for TunerManager {
    fn default() -> Self {
        unreachable!();
    }
}

// query tuners

pub struct QueryTunersMessage;

impl fmt::Display for QueryTunersMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "QueryTuners")
    }
}

impl Message for QueryTunersMessage {
    type Result = Result<Vec<MirakurunTuner>, Error>;
}

impl Handler<QueryTunersMessage> for TunerManager {
    type Result = Result<Vec<MirakurunTuner>, Error>;

    fn handle(
        &mut self,
        msg: QueryTunersMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        let tuners: Vec<MirakurunTuner> = self.tuners
            .iter()
            .map(|tuner| tuner.get_model())
            .collect();
        Ok(tuners)
    }
}

// start streaming

pub struct StartStreamingMessage {
    pub channel_type: ChannelType,
    pub channel: String,
    pub user: TunerUser,
}

impl fmt::Display for StartStreamingMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StartStreaming {}/{} to {}",
               self.channel_type, self.channel, self.user)
    }
}

impl Message for StartStreamingMessage {
    type Result = Result<MpegTsStream, Error>;
}

impl Handler<StartStreamingMessage> for TunerManager {
    type Result = ActorResponse<Self, MpegTsStream, Error>;

    fn handle(
        &mut self,
        msg: StartStreamingMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);

        let subscription = match self.activate_tuner(
            msg.channel_type, msg.channel, msg.user) {
            Ok(broadcaster) => broadcaster,
            Err(err) => return ActorResponse::reply(Err(Error::from(err))),
        };

        let fut = actix::fut::wrap_future::<_, Self>(
            subscription.broadcaster.send(SubscribeMessage {
                id: subscription.id
            }))
            .map(move |result, act, _| {
                if result.is_ok() {
                    log::info!("{}: Started streaming", subscription.id);
                } else {
                    log::error!("{}: Broadcaster may have stopped",
                                subscription.id);
                    act.deactivate_tuner(subscription.id);
                }
                result.map_err(Error::from)
            });

        ActorResponse::r#async(fut)
    }
}

// stop streaming

pub struct StopStreamingMessage {
    pub id: TunerSubscriptionId,
}

impl fmt::Display for StopStreamingMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StopStreaming {}", self.id)
    }
}

impl Message for StopStreamingMessage {
    type Result = ();
}

impl Handler<StopStreamingMessage> for TunerManager {
    type Result = ();

    fn handle(
        &mut self,
        msg: StopStreamingMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        self.stop_streaming(msg.id)
    }
}

// tuner

struct Tuner {
    index: usize,
    name: String,
    channel_types: Vec<ChannelType>,
    command: String,
    activity: TunerActivity,
}

impl Tuner {
    fn new(
        index: usize,
        config: &TunerConfig,
    ) -> Self {
        Tuner {
            index,
            name: config.name.clone(),
            channel_types: config.channel_types.clone(),
            command: config.command.clone(),
            activity: TunerActivity::Inactive,
        }
    }

    fn is_active(&self) -> bool {
        self.activity.is_active()
    }

    fn is_available(&self) -> bool {
        self.activity.is_inactive()
    }

    fn is_supported_type(&self, channel_type: ChannelType) -> bool {
        self.channel_types.contains(&channel_type)
    }

    fn is_available_for(&self, channel_type: ChannelType) -> bool {
        self.is_available() && self.is_supported_type(channel_type)
    }

    fn is_reuseable(
        &self, channel_type: ChannelType, channel: &str) -> bool {
        self.activity.is_reuseable(channel_type, channel)
    }

    fn can_grab(&self, priority: TunerUserPriority) -> bool {
        priority.is_grab() || self.activity.can_grab(priority)
    }

    fn activate(
        &mut self,
        channel_type: ChannelType,
        channel: String,
    ) -> Result<(), Error> {
        let command = self.make_command(channel_type, &channel)?;
        self.activity.activate(
            self.index, channel_type, channel.clone(), command)
    }

    fn deactivate(&mut self) {
        self.activity.deactivate();
    }

    fn subscribe(&mut self, user: TunerUser) -> TunerSubscription {
        self.activity.subscribe(user)
    }

    fn stop_streaming(
        &mut self,
        id: TunerSubscriptionId,
    ) -> Result<(), Error> {
        let num_users = self.activity.stop_streaming(id)?;
        if num_users == 0 {
            self.deactivate();
        }
        Ok(())
    }

    fn get_model(&self) -> MirakurunTuner {
        let (command, pid, users) = self.activity.get_models();

        MirakurunTuner {
            index: self.index,
            name: self.name.clone(),
            channel_types: self.channel_types.clone(),
            command,
            pid,
            users,
            is_available: true,
            is_remote: false,
            is_free: self.is_available(),
            is_using: !self.is_available(),
            is_fault: false,
        }
    }

    fn make_command(
        &self,
        channel_type: ChannelType,
        channel: &str,
    ) -> Result<String, Error> {
        let template = mustache::compile_str(&self.command)?;
        let data = mustache::MapBuilder::new()
            .insert("channel_type", &channel_type)?
            .insert_str("channel", channel)
            .insert_str("duration", "-")
            .build();
        Ok(template.render_data_to_string(&data)?)
    }
}

// activity

enum TunerActivity {
    Inactive,
    Active(TunerSession),
}

impl TunerActivity {
    fn activate(
        &mut self,
        tuner_index: usize,
        channel_type: ChannelType,
        channel: String,
        command: String
    ) -> Result<(), Error> {
        match self {
            Self::Inactive => {
                let session = TunerSession::new(
                    tuner_index, channel_type, channel, command)?;
                *self = Self::Active(session);
                Ok(())
            }
            Self::Active(_) => panic!("Must be deactivated before activating"),
        }
    }

    fn deactivate(&mut self) {
        *self = Self::Inactive;
    }

    fn is_active(&self) -> bool {
        match self {
            Self::Inactive => false,
            Self::Active(_) => true,
        }
    }

    fn is_inactive(&self) -> bool {
        !self.is_active()
    }

    fn is_reuseable(&self, channel_type: ChannelType, channel: &str) -> bool {
        match self {
            Self::Inactive => false,
            Self::Active(session) =>
                session.is_reuseable(channel_type, channel),
        }
    }

    fn subscribe(&mut self, user: TunerUser) -> TunerSubscription {
        match self {
            Self::Inactive => panic!("Must be activated before subscribing"),
            Self::Active(session) => session.subscribe(user),
        }
    }

    fn stop_streaming(
        &mut self,
        id: TunerSubscriptionId,
    ) -> Result<usize, Error> {
        match self {
            Self::Inactive => Err(Error::SessionNotFound),
            Self::Active(session) => session.stop_streaming(id),
        }
    }

    fn can_grab(&self, priority: TunerUserPriority) -> bool {
        match self {
            Self::Inactive => true,
            Self::Active(session) => session.can_grab(priority),
        }
    }

    fn get_models(
        &self
    ) -> (Option<String>, Option<u32>, Vec<MirakurunTunerUser>) {
        match self {
            Self::Inactive => (None, None, Vec::new()),
            Self::Active(session) => session.get_models(),
        }
    }
}

// session

struct TunerSession {
    id: TunerSessionId,
    channel_type: ChannelType,
    channel: String,
    command: String,
    // Used for closing the tuner in order to take over the right to use it.
    process: Child,
    broadcaster: Addr<Broadcaster>,
    subscribers: HashMap<u32, TunerUser>,
    next_serial_number: u32,
}

impl TunerSession {
    fn new(
        tuner_index: usize,
        channel_type: ChannelType,
        channel: String,
        command: String
    ) -> Result<TunerSession, Error> {
        let mut process = command_util::spawn_process(&command, Stdio::null())?;
        let id = TunerSessionId { tuner_index, tuner_pid: process.id() };
        log::debug!("{}: Spawned {}: `{}`", id, process.id(), command);

        let reader = tokio_snippet::stdio(process.stdout.take())?.unwrap();
        let broadcaster = Broadcaster::create(|ctx| {
            Broadcaster::new(id.clone(), reader, ctx)
        });

        log::info!("{}: Activated with {} {}", id, channel_type, channel);

        Ok(TunerSession {
            id, channel_type, channel, command, process, broadcaster,
            subscribers: HashMap::new(), next_serial_number: 1
        })
    }

    fn is_reuseable(&self, channel_type: ChannelType, channel: &str) -> bool {
        self.channel_type == channel_type && self.channel == channel
    }

    fn subscribe(&mut self, user: TunerUser) -> TunerSubscription {
        let serial_number = self.next_serial_number;
        self.next_serial_number += 1;

        let id = TunerSubscriptionId { session_id: self.id, serial_number };
        log::info!("{}: Subscribed: {}", id, user);
        self.subscribers.insert(serial_number, user);

        TunerSubscription { id, broadcaster: self.broadcaster.clone() }
    }

    fn can_grab(&self, priority: TunerUserPriority) -> bool {
        self.subscribers
            .values()
            .all(|user| priority > user.priority)
    }

    fn stop_streaming(
        &mut self,
        id: TunerSubscriptionId
    ) -> Result<usize, Error> {
        if self.id != id.session_id {
            log::warn!("Session ID unmatched, {} was probably deactivated",
                       id.session_id);
            return Err(Error::SessionNotFound);
        }
        match self.subscribers.remove(&id.serial_number) {
            Some(user) => log::info!("{}: Unsubscribed: {}", id, user),
            None => log::warn!("{}: Not subscribed", id),
        }
        self.broadcaster.do_send(UnsubscribeMessage { id });
        Ok(self.subscribers.len())
    }

    fn get_models(
        &self
    ) -> (Option<String>, Option<u32>, Vec<MirakurunTunerUser>) {
        (
            Some(self.command.clone()),
            Some(self.process.id()),
            self.subscribers.values().map(|user| user.get_model()).collect(),
        )
    }
}

impl Drop for TunerSession {
    fn drop(&mut self) {
        // Always kill the process and ignore the error.  Because there is no
        // method to check whether the process is alive or dead.
        let _ = self.process.kill();
        let _ = self.process.wait();
        log::debug!("{}: Killed {}: {}",
                    self.id, self.process.id(), self.command);
        log::info!("{}: Deactivated", self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matches::assert_matches;

    #[actix_rt::test]
    async fn test_tuner_is_active() {
        let config = create_config("true".to_string());
        let mut tuner = Tuner::new(0, &config);

        assert!(!tuner.is_active());

        let result = tuner.activate(ChannelType::GR, String::new());
        assert!(result.is_ok());

        assert!(tuner.is_active());
    }

    #[actix_rt::test]
    async fn test_tuner_activate() {
        {
            let config = create_config("true".to_string());
            let mut tuner = Tuner::new(0, &config);
            let result = tuner.activate(ChannelType::GR, String::new());
            assert!(result.is_ok());
        }

        {
            let config = create_config("cmd '".to_string());
            let mut tuner = Tuner::new(0, &config);
            let result = tuner.activate(ChannelType::GR, String::new());
            assert_matches!(result, Err(Error::CommandFailed(
                command_util::Error::UnableToParse(_))));
        }

        {
            let config = create_config("no-such-command".to_string());
            let mut tuner = Tuner::new(0, &config);
            let result = tuner.activate(ChannelType::GR, String::new());
            assert_matches!(result, Err(Error::CommandFailed(
                command_util::Error::UnableToSpawn(..))));
        }
    }

    #[actix_rt::test]
    async fn test_tuner_stop_streaming() {
        let config = create_config("true".to_string());
        let mut tuner = Tuner::new(0, &config);
        let result = tuner.stop_streaming(Default::default());
        assert_matches!(result, Err(Error::SessionNotFound));

        let result = tuner.activate(ChannelType::GR, String::new());
        assert!(result.is_ok());
        let subscription = tuner.subscribe(TunerUser {
            info: TunerUserInfo::Web { remote: None, agent: None },
            priority: 0.into(),
        });

        let result = tuner.stop_streaming(Default::default());
        assert_matches!(result, Err(Error::SessionNotFound));

        let result = tuner.stop_streaming(subscription.id);
        assert_matches!(result, Ok(()));
    }

    #[actix_rt::test]
    async fn test_tuner_can_grab() {
        let config = create_config("true".to_string());
        let mut tuner = Tuner::new(0, &config);
        assert!(tuner.can_grab(0.into()));

        tuner.activate(ChannelType::GR, "1".to_string()).unwrap();
        tuner.subscribe(create_user(0.into()));

        assert!(!tuner.can_grab(0.into()));
        assert!(tuner.can_grab(1.into()));
        assert!(tuner.can_grab(2.into()));
        assert!(tuner.can_grab(TunerUserPriority::GRAB));

        tuner.subscribe(create_user(1.into()));

        assert!(!tuner.can_grab(0.into()));
        assert!(!tuner.can_grab(1.into()));
        assert!(tuner.can_grab(2.into()));
        assert!(tuner.can_grab(TunerUserPriority::GRAB));

        tuner.subscribe(create_user(TunerUserPriority::GRAB));

        assert!(!tuner.can_grab(0.into()));
        assert!(!tuner.can_grab(1.into()));
        assert!(!tuner.can_grab(2.into()));
        assert!(tuner.can_grab(TunerUserPriority::GRAB));
    }

    #[actix_rt::test]
    async fn test_tuner_reactivate() {
        let config = create_config("true".to_string());
        let mut tuner = Tuner::new(0, &config);
        tuner.activate(ChannelType::GR, "1".to_string()).ok();

        tuner.deactivate();
        let result = tuner.activate(ChannelType::GR, "2".to_string());
        assert!(result.is_ok());
    }

    fn create_config(command: String) -> TunerConfig {
        TunerConfig {
            name: String::new(),
            channel_types: vec![ChannelType::GR],
            command,
            disabled: false,
        }
    }

    fn create_user(priority: TunerUserPriority) -> TunerUser {
        TunerUser {
            info: TunerUserInfo::Job { name: "test".to_string() },
            priority
        }
    }
}
