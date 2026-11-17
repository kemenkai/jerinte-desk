// 24FPS (actually 23.976FPS) is what video professionals ages ago determined to be the
// slowest playback rate that still looks smooth enough to feel real.
// Our eyes can see a slight difference and even though 30FPS actually shows
// more information and is more realistic.
// 60FPS is commonly used in game, teamviewer 12 support this for video editing user.

// how to capture with mouse cursor:
// https://docs.microsoft.com/zh-cn/windows/win32/direct3ddxgi/desktop-dup-api?redirectedfrom=MSDN

// RECORD: The following Project has implemented audio capture, hardware codec and mouse cursor drawn.
// https://github.com/PHZ76/DesktopSharing

// dxgi memory leak issue
// https://stackoverflow.com/questions/47801238/memory-leak-in-creating-direct2d-device
// but per my test, it is more related to AcquireNextFrame,
// https://forums.developer.nvidia.com/t/dxgi-outputduplication-memory-leak-when-using-nv-but-not-amd-drivers/108582

// to-do:
// https://slhck.info/video/2017/03/01/rate-control.html

use super::{display_service::check_display_changed, service::ServiceTmpl, video_qos::VideoQoS, *};
#[cfg(target_os = "linux")]
use crate::common::SimpleCallOnReturn;
#[cfg(target_os = "linux")]
use crate::platform::linux::is_x11;
use crate::privacy_mode::{get_privacy_mode_conn_id, INVALID_PRIVACY_MODE_CONN_ID};
#[cfg(windows)]
use crate::{
    platform::windows::is_process_consent_running,
    privacy_mode::{is_current_privacy_mode_impl, PRIVACY_MODE_IMPL_WIN_MAG},
    ui_interface::is_installed,
};
use hbb_common::{
    anyhow::anyhow,
    config,
    tokio::sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        Mutex as TokioMutex,
    },
};
#[cfg(feature = "hwcodec")]
use scrap::hwcodec::{HwRamEncoder, HwRamEncoderConfig};
#[cfg(feature = "vram")]
use scrap::vram::{VRamEncoder, VRamEncoderConfig};
#[cfg(not(windows))]
use scrap::Capturer;
use scrap::{
    aom::AomEncoderConfig,
    codec::{Encoder, EncoderCfg},
    record::{Recorder, RecorderContext},
    vpxcodec::{VpxEncoderConfig, VpxVideoCodecId},
    CodecFormat, Display, EncodeInput, TraitCapturer, TraitPixelBuffer,
};
#[cfg(windows)]
use std::io::ErrorKind::ConnectionReset;
#[cfg(windows)]
use std::sync::Once;
use std::{
    collections::HashSet,
    io::ErrorKind::WouldBlock,
    ops::{Deref, DerefMut},
    time::{self, Duration, Instant},
};

pub const OPTION_REFRESH: &'static str = "refresh";

mod static_refresh;

#[cfg(windows)]
const DXGI_RECOVERY_LIMIT: usize = 3;
#[cfg(windows)]
const DXGI_RECOVERY_WINDOW: Duration = Duration::from_secs(10);
#[cfg(windows)]
const DXGI_RECOVERY_FRAME_GRACE: Duration = Duration::from_secs(2);
/// The attempt a restart after a change of our virtual displays counts as: none, counted ones
/// starting at 1.
#[cfg(windows)]
const LAYOUT_CHANGE: usize = 0;

#[cfg(windows)]
struct DxgiRecoveryState {
    attempts: usize,
    window_started: Option<Instant>,
    restart_pending: bool,
    fallback_pending: bool,
}

#[cfg(windows)]
impl DxgiRecoveryState {
    fn new() -> Self {
        Self {
            attempts: 0,
            window_started: None,
            restart_pending: false,
            fallback_pending: false,
        }
    }

    fn next_attempt(&mut self) -> Option<usize> {
        if self
            .window_started
            .map(|started| started.elapsed() > DXGI_RECOVERY_WINDOW)
            .unwrap_or(true)
        {
            self.attempts = 0;
            self.window_started = Some(Instant::now());
        }
        if self.attempts >= DXGI_RECOVERY_LIMIT {
            self.fallback_pending = true;
            return None;
        }
        self.attempts += 1;
        self.restart_pending = true;
        Some(self.attempts)
    }

    /// A restart for an access lost through a change of our virtual displays: not counted
    /// (`LAYOUT_CHANGE`), the replacement capturer still given time for its first frame.
    fn restart_after_layout_change(&mut self) -> usize {
        self.restart_pending = true;
        LAYOUT_CHANGE
    }

    fn take_restart_pending(&mut self) -> bool {
        std::mem::take(&mut self.restart_pending)
    }

    fn take_fallback_pending(&mut self) -> bool {
        std::mem::take(&mut self.fallback_pending)
    }
}

type FrameFetchedNotifierSender = UnboundedSender<(i32, Option<Instant>)>;
type FrameFetchedNotifierReceiver = Arc<TokioMutex<UnboundedReceiver<(i32, Option<Instant>)>>>;

lazy_static::lazy_static! {
    static ref FRAME_FETCHED_NOTIFIERS: Mutex<HashMap<usize, (FrameFetchedNotifierSender, FrameFetchedNotifierReceiver)>> = Mutex::new(HashMap::default());

    // display_idx -> set of conn id.
    // Used to record which connections need to be notified when
    // 1. A new frame is received from a web client.
    //   Because web client does not send the display index in message `VideoReceived`.
    // 2. The client is closing.
    static ref DISPLAY_CONN_IDS: Arc<Mutex<HashMap<usize, HashSet<i32>>>> = Default::default();
    pub static ref VIDEO_QOS: Arc<Mutex<VideoQoS>> = Default::default();
    pub static ref IS_UAC_RUNNING: Arc<Mutex<bool>> = Default::default();
    pub static ref IS_FOREGROUND_WINDOW_ELEVATED: Arc<Mutex<bool>> = Default::default();
    static ref SCREENSHOTS: Mutex<HashMap<(VideoSource, usize), Screenshot>> = Default::default();
}

struct Screenshot {
    sid: String,
    tx: Sender,
    restore_vram: bool,
}

#[inline]
pub fn notify_video_frame_fetched(display_idx: usize, conn_id: i32, frame_tm: Option<Instant>) {
    if let Some(notifier) = FRAME_FETCHED_NOTIFIERS.lock().unwrap().get(&display_idx) {
        notifier.0.send((conn_id, frame_tm)).ok();
    }
}

#[inline]
pub fn notify_video_frame_fetched_by_conn_id(conn_id: i32, frame_tm: Option<Instant>) {
    let vec_display_idx: Vec<usize> = {
        let display_conn_ids = DISPLAY_CONN_IDS.lock().unwrap();
        display_conn_ids
            .iter()
            .filter_map(|(display_idx, conn_ids)| {
                if conn_ids.contains(&conn_id) {
                    Some(*display_idx)
                } else {
                    None
                }
            })
            .collect()
    };
    let notifiers = FRAME_FETCHED_NOTIFIERS.lock().unwrap();
    for display_idx in vec_display_idx {
        if let Some(notifier) = notifiers.get(&display_idx) {
            notifier.0.send((conn_id, frame_tm)).ok();
        }
    }
}

struct VideoFrameController {
    display_idx: usize,
    cur: Instant,
    send_conn_ids: HashSet<i32>,
}

impl VideoFrameController {
    fn new(display_idx: usize) -> Self {
        Self {
            display_idx,
            cur: Instant::now(),
            send_conn_ids: HashSet::new(),
        }
    }

    fn reset(&mut self) {
        self.send_conn_ids.clear();
    }

    fn set_send(&mut self, tm: Instant, conn_ids: HashSet<i32>) {
        if !conn_ids.is_empty() {
            self.cur = tm;
            self.send_conn_ids = conn_ids;
            DISPLAY_CONN_IDS
                .lock()
                .unwrap()
                .insert(self.display_idx, self.send_conn_ids.clone());
        }
    }

    #[tokio::main(flavor = "current_thread")]
    async fn try_wait_next(&mut self, fetched_conn_ids: &mut HashSet<i32>, timeout_millis: u64) {
        if self.send_conn_ids.is_empty() {
            return;
        }

        let timeout_dur = Duration::from_millis(timeout_millis as u64);
        let receiver = {
            match FRAME_FETCHED_NOTIFIERS
                .lock()
                .unwrap()
                .get(&self.display_idx)
            {
                Some(notifier) => notifier.1.clone(),
                None => {
                    return;
                }
            }
        };
        let mut receiver_guard = receiver.lock().await;
        match tokio::time::timeout(timeout_dur, receiver_guard.recv()).await {
            Err(_) => {
                // break if timeout
                // log::error!("blocking wait frame receiving timeout {}", timeout_millis);
            }
            Ok(Some((id, instant))) => {
                if let Some(tm) = instant {
                    log::trace!("Channel recv latency: {}", tm.elapsed().as_secs_f32());
                }
                fetched_conn_ids.insert(id);
            }
            Ok(None) => {
                // this branch would never be reached
            }
        }
        while !receiver_guard.is_empty() {
            if let Some((id, instant)) = receiver_guard.recv().await {
                if let Some(tm) = instant {
                    log::trace!("Channel recv latency: {}", tm.elapsed().as_secs_f32());
                }
                fetched_conn_ids.insert(id);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum VideoSource {
    Monitor,
    Camera,
}

impl VideoSource {
    pub fn service_name_prefix(&self) -> &'static str {
        match self {
            VideoSource::Monitor => "monitor",
            VideoSource::Camera => "camera",
        }
    }

    pub fn is_monitor(&self) -> bool {
        matches!(self, VideoSource::Monitor)
    }

    pub fn is_camera(&self) -> bool {
        matches!(self, VideoSource::Camera)
    }
}

#[derive(Clone)]
pub struct VideoService {
    sp: GenericService,
    idx: usize,
    source: VideoSource,
    #[cfg(windows)]
    dxgi_recovery_state: Arc<Mutex<DxgiRecoveryState>>,
}

impl Deref for VideoService {
    type Target = ServiceTmpl<ConnInner>;

    fn deref(&self) -> &Self::Target {
        &self.sp
    }
}

impl DerefMut for VideoService {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.sp
    }
}

pub fn get_service_name(source: VideoSource, idx: usize) -> String {
    format!("{}{}", source.service_name_prefix(), idx)
}

pub fn new(source: VideoSource, idx: usize) -> GenericService {
    let _ = FRAME_FETCHED_NOTIFIERS
        .lock()
        .unwrap()
        .entry(idx)
        .or_insert_with(|| {
            let (tx, rx) = unbounded_channel();
            (tx, Arc::new(TokioMutex::new(rx)))
        });
    let vs = VideoService {
        sp: GenericService::new(get_service_name(source, idx), true),
        idx,
        source,
        #[cfg(windows)]
        dxgi_recovery_state: Arc::new(Mutex::new(DxgiRecoveryState::new())),
    };
    #[cfg(all(windows, feature = "vram"))]
    {
        let name = vs.sp.name();
        vs.sp.set_on_unwatched(move || {
            forget_vram_marks(&name, (source, idx));
            // Not on the unsubscribing thread, which holds the server's lock: working the encodings
            // out anew enumerates the displays.
            std::thread::spawn(|| Encoder::update(scrap::codec::EncodingUpdate::Check));
        });
    }
    GenericService::run(&vs, run);
    vs.sp
}

// Capturer object is expensive, avoiding to create it frequently.
fn create_capturer(
    privacy_mode_id: i32,
    display: Display,
    _current: usize,
    _portable_service_running: bool,
) -> ResultType<Box<dyn TraitCapturer>> {
    #[cfg(not(windows))]
    let c: Option<Box<dyn TraitCapturer>> = None;
    #[cfg(windows)]
    let mut c: Option<Box<dyn TraitCapturer>> = None;
    if privacy_mode_id > 0 {
        #[cfg(windows)]
        {
            // Windows Mode 1 can cover every local monitor with overlay windows,
            // but the legacy magnifier capture backend is still single-monitor
            // constrained. Keep display-switch gating aligned with that backend
            // limit, not just the overlay coverage.
            if let Some(c1) = crate::privacy_mode::win_mag::create_capturer(
                privacy_mode_id,
                display.origin(),
                display.width(),
                display.height(),
            )? {
                c = Some(Box::new(c1));
            }
        }
    }

    match c {
        Some(c1) => return Ok(c1),
        None => {
            #[cfg(windows)]
            {
                log::debug!("Create capturer dxgi|gdi");
                return crate::portable_service::client::create_capturer(
                    _current,
                    display,
                    _portable_service_running,
                );
            }
            #[cfg(not(windows))]
            {
                log::debug!("Create capturer from scrap");
                return Ok(Box::new(
                    Capturer::new(display).with_context(|| "Failed to create capturer")?,
                ));
            }
        }
    };
}

// This function works on privacy mode. Windows only for now.
pub fn test_create_capturer(
    privacy_mode_id: i32,
    display_idx: usize,
    timeout_millis: u64,
) -> String {
    let test_begin = Instant::now();
    loop {
        let err = match super::display_service::all_displays() {
            Ok(mut displays) => {
                if displays.len() <= display_idx {
                    anyhow!(
                        "Failed to get display {}, the displays' count is {}",
                        display_idx,
                        displays.len()
                    )
                } else {
                    let display = displays.remove(display_idx);
                    match create_capturer(privacy_mode_id, display, display_idx, false) {
                        Ok(_) => return "".to_owned(),
                        Err(e) => e,
                    }
                }
            }
            Err(e) => e.into(),
        };
        if test_begin.elapsed().as_millis() >= timeout_millis as _ {
            return err.to_string();
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

// Note: This function is extremely expensive, do not call it frequently.
#[cfg(windows)]
fn check_uac_switch(privacy_mode_id: i32, capturer_privacy_mode_id: i32) -> ResultType<()> {
    if capturer_privacy_mode_id != INVALID_PRIVACY_MODE_CONN_ID
        && is_current_privacy_mode_impl(PRIVACY_MODE_IMPL_WIN_MAG)
    {
        if !is_installed() {
            if privacy_mode_id != capturer_privacy_mode_id {
                if !is_process_consent_running()? {
                    bail!("consent.exe is not running");
                }
            }
            if is_process_consent_running()? {
                bail!("consent.exe is running");
            }
        }
    }
    Ok(())
}

pub(super) struct CapturerInfo {
    pub origin: (i32, i32),
    pub width: usize,
    pub height: usize,
    pub ndisplay: usize,
    pub current: usize,
    pub privacy_mode_id: i32,
    pub _capturer_privacy_mode_id: i32,
    pub capturer: Box<dyn TraitCapturer>,
}

impl Deref for CapturerInfo {
    type Target = Box<dyn TraitCapturer>;

    fn deref(&self) -> &Self::Target {
        &self.capturer
    }
}

impl DerefMut for CapturerInfo {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.capturer
    }
}

fn get_capturer_monitor(
    current: usize,
    portable_service_running: bool,
) -> ResultType<CapturerInfo> {
    #[cfg(target_os = "linux")]
    {
        if !is_x11() {
            return super::wayland::get_capturer_for_display(current);
        }
    }

    let mut displays = super::display_service::all_displays()?;
    let ndisplay = displays.len();
    if ndisplay <= current {
        bail!(
            "Failed to get display {}, displays len: {}",
            current,
            ndisplay
        );
    }

    let display = displays.remove(current);

    #[cfg(target_os = "linux")]
    if let Display::X11(inner) = &display {
        if let Err(err) = inner.get_shm_status() {
            log::warn!(
                "MIT-SHM extension not working properly on select X11 server: {:?}",
                err
            );
        }
    }

    let (origin, width, height) = (display.origin(), display.width(), display.height());
    let name = display.name();
    log::debug!(
        "#displays={}, current={}, origin: {:?}, width={}, height={}, cpus={}/{}, name:{}",
        ndisplay,
        current,
        &origin,
        width,
        height,
        num_cpus::get_physical(),
        num_cpus::get(),
        &name,
    );

    let privacy_mode_id = get_privacy_mode_conn_id().unwrap_or(INVALID_PRIVACY_MODE_CONN_ID);
    #[cfg(not(windows))]
    let capturer_privacy_mode_id = privacy_mode_id;
    #[cfg(windows)]
    let mut capturer_privacy_mode_id = privacy_mode_id;
    #[cfg(windows)]
    {
        if capturer_privacy_mode_id != INVALID_PRIVACY_MODE_CONN_ID
            && is_current_privacy_mode_impl(PRIVACY_MODE_IMPL_WIN_MAG)
        {
            if !is_installed() {
                if is_process_consent_running()? {
                    capturer_privacy_mode_id = INVALID_PRIVACY_MODE_CONN_ID;
                }
            }
        }
    }
    log::debug!(
        "Try create capturer with capturer privacy mode id {}",
        capturer_privacy_mode_id,
    );

    if privacy_mode_id != INVALID_PRIVACY_MODE_CONN_ID {
        if privacy_mode_id != capturer_privacy_mode_id {
            log::info!("In privacy mode, but show UAC prompt window for now");
        } else {
            log::info!("In privacy mode, the peer side cannot watch the screen");
        }
    }
    let capturer = create_capturer(
        capturer_privacy_mode_id,
        display,
        current,
        portable_service_running,
    )?;
    Ok(CapturerInfo {
        origin,
        width,
        height,
        ndisplay,
        current,
        privacy_mode_id,
        _capturer_privacy_mode_id: capturer_privacy_mode_id,
        capturer,
    })
}

fn get_capturer_camera(current: usize) -> ResultType<CapturerInfo> {
    let cameras = camera::Cameras::get_sync_cameras();
    let ncamera = cameras.len();
    if ncamera <= current {
        bail!("Failed to get camera {}, cameras len: {}", current, ncamera,);
    }
    let Some(camera) = cameras.get(current) else {
        bail!(
            "Camera of index {} doesn't exist or platform not supported",
            current
        );
    };
    let capturer = camera::Cameras::get_capturer(current)?;
    let (width, height) = (camera.width as usize, camera.height as usize);
    let origin = (camera.x as i32, camera.y as i32);
    let name = &camera.name;
    let privacy_mode_id = get_privacy_mode_conn_id().unwrap_or(INVALID_PRIVACY_MODE_CONN_ID);
    let _capturer_privacy_mode_id = privacy_mode_id;
    log::debug!(
        "#cameras={}, current={}, origin: {:?}, width={}, height={}, cpus={}/{}, name:{}",
        ncamera,
        current,
        &origin,
        width,
        height,
        num_cpus::get_physical(),
        num_cpus::get(),
        name,
    );
    return Ok(CapturerInfo {
        origin,
        width,
        height,
        ndisplay: ncamera,
        current,
        privacy_mode_id,
        _capturer_privacy_mode_id: privacy_mode_id,
        capturer,
    });
}
fn get_capturer(
    source: VideoSource,
    current: usize,
    portable_service_running: bool,
) -> ResultType<CapturerInfo> {
    match source {
        VideoSource::Monitor => get_capturer_monitor(current, portable_service_running),
        VideoSource::Camera => get_capturer_camera(current),
    }
}

thread_local! {
    // What this thread's video loop encoded since `check_qos` last read it, in bytes and in
    // frames: a frame the encoder left out is in neither.
    static SENT_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SENT_FRAMES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    // Fork: the rate a software encoder is paced to (`VideoQoS::pace_kbps`, 0 for none), what it
    // sent and has not paid off at that rate yet, in bits, and up to when it was paid off.
    static PACE_KBPS: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
    static PACE_BITS: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
    static PACE_AT: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    // How long this thread's encoder takes for a frame, averaged over the last frames.
    static ENCODE_TIME: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

/// Counts a video frame as encoded by this thread's video loop, for `VideoQoS` to set against
/// what the viewer says it received.
fn count_sent(msg: &Message) {
    use hbb_common::protobuf::Message as _;
    let bytes = msg.compute_size() as usize;
    SENT_BYTES.with(|sent| sent.set(sent.get() + bytes));
    SENT_FRAMES.with(|frames| frames.set(frames.get() + 1));
    // Paid off up to now first: a frame does not use the time before it.
    pay_off(Instant::now());
    PACE_BITS.with(|bits| bits.set(bits.get() + bytes as f64 * 8.0));
}

/// Fork: what this thread's encoder sent is paid off at the rate it is paced to, up to `now`.
fn pay_off(now: Instant) {
    let rate = PACE_KBPS.with(|kbps| kbps.get()) * 1000.0;
    let since = PACE_AT.with(|at| at.replace(Some(now)));
    PACE_BITS.with(|bits| {
        let paid = since.map_or(0.0, |at| rate * now.saturating_duration_since(at).as_secs_f64());
        bits.set(if rate > 0.0 {
            (bits.get() - paid).max(0.0)
        } else {
            0.0
        });
    });
}

/// Fork: the rate this thread's software encoder is paced to from now on (`VideoQoS::pace_kbps`,
/// `None` for none).
fn set_pace(kbps: Option<f32>) {
    pay_off(Instant::now());
    PACE_KBPS.with(|rate| rate.set(kbps.map_or(0.0, |kbps| kbps.max(0.0) as f64)));
}

/// Fork: how long what this thread's encoder sent over its pace takes to pay off. The video
/// loop takes no new picture before then: frames that cannot be made smaller come at the pace,
/// and a frame twice the pace's share of a frame interval sets the next one back by one.
fn pace_wait() -> Duration {
    pay_off(Instant::now());
    let rate = PACE_KBPS.with(|kbps| kbps.get()) * 1000.0;
    if rate <= 0.0 {
        return Duration::ZERO;
    }
    Duration::from_secs_f64(PACE_BITS.with(|bits| bits.get()) / rate)
}

/// Encodes a frame, and keeps how long this thread's encoder takes for one.
fn timed_encode(encoder: &mut Encoder, frame: EncodeInput, ms: i64) -> ResultType<VideoFrame> {
    let started = Instant::now();
    let result = encoder.encode_to_message(frame, ms);
    note_encode_time(started.elapsed());
    result
}

/// Takes the time a frame took into this thread's average.
fn note_encode_time(took: Duration) {
    ENCODE_TIME.with(|time| {
        time.set(Some(match time.get() {
            // Up at once, down slowly: the frames of a still screen are quick to encode, and a
            // few of them must not hide what the frames of a moving one take.
            Some(average) if took >= average => average.mul_f32(0.5) + took.mul_f32(0.5),
            Some(average) => average.mul_f32(0.95) + took.mul_f32(0.05),
            None => took,
        }))
    });
}

/// The pace a slow software encoder is held to at the slowest: thirty frames a second, what the
/// presets ask for and every machine was asked for before higher frame rates became the default.
const SLOWEST_PACE: Duration = Duration::from_millis(1000 / 30);

/// The time from one frame to the next, of which `spf` is what the controller asks for.
///
/// Above thirty frames a second a software encoder is given at least twice the time it takes for
/// a frame. Encoding at a frame rate it can hardly keep would hold the processor at it without a
/// pause, and nothing would bring the frame rate down: the controller follows the network, which
/// a slow encoder does not load. Up to thirty nothing changes, and a hardware encoder is not held
/// back at all.
fn frame_interval(spf: Duration, hardware: bool) -> Duration {
    if hardware {
        return spf;
    }
    match ENCODE_TIME.with(|time| time.get()) {
        Some(average) => spf.max((average * 2).min(SLOWEST_PACE)),
        None => spf,
    }
}

fn run(vs: VideoService) -> ResultType<()> {
    let mut _raii = Raii::new(vs.idx, vs.sp.name());
    // A new encoder: what the last one took and left unread says nothing of it.
    SENT_BYTES.with(|bytes| bytes.set(0));
    SENT_FRAMES.with(|frames| frames.set(0));
    set_pace(None);
    ENCODE_TIME.with(|time| time.set(None));
    // Wayland only support one video capturer for now. It is ok to call ensure_inited() here.
    //
    // ensure_inited() is needed because clear() may be called.
    // to-do: wayland ensure_inited should pass current display index.
    // But for now, we do not support multi-screen capture on wayland.
    #[cfg(target_os = "linux")]
    super::wayland::ensure_inited()?;
    #[cfg(target_os = "linux")]
    let _wayland_call_on_ret = {
        // Increment active display count when starting
        let _display_count = super::wayland::increment_active_display_count();

        SimpleCallOnReturn {
            b: true,
            f: Box::new(|| {
                // Decrement active display count and only clear if this was the last display
                let remaining_count = super::wayland::decrement_active_display_count();
                if remaining_count == 0 {
                    super::wayland::clear();
                }
            }),
        }
    };

    #[cfg(windows)]
    let last_portable_service_running = crate::portable_service::client::running();
    #[cfg(not(windows))]
    let last_portable_service_running = false;

    let display_idx = vs.idx;
    #[cfg(windows)]
    let dxgi_recovery_state = vs.dxgi_recovery_state.clone();
    let sp = vs.sp;
    // Not while one of our virtual displays is being plugged in, resized or removed: a capturer
    // made on a layout half changed loses its access again as the change goes on. Bounded, as the
    // settling is (`crate::vdisplay::layout_settling`).
    #[cfg(windows)]
    while vs.source.is_monitor() && crate::vdisplay::layout_settling() && sp.ok() {
        std::thread::sleep(Duration::from_millis(100));
    }
    // Read before the capturer is made: a change of ours starting any later shows in it.
    #[cfg(windows)]
    let made_at_layout = crate::vdisplay::layout_generation();
    // A mark this capture leaves as it ends, nobody watching any more, would not be cleared by the
    // unsubscribe that already happened (`forget_vram_marks`).
    #[cfg(all(windows, feature = "vram"))]
    let _unwatched = ForgetVramMarksIfUnwatched(sp.clone(), (vs.source, display_idx));
    let mut c = get_capturer(vs.source, display_idx, last_portable_service_running)?;
    #[cfg(windows)]
    // ACCESS_LOST marks the next successful capturer creation as a recovery. This timestamp is
    // consumed once and temporarily holds off the normal WouldBlock-to-GDI fallback, giving the
    // replacement DXGI capturer time to produce its first frame. Normal startup is unaffected.
    let dxgi_recovery_started = dxgi_recovery_state
        .lock()
        .unwrap()
        .take_restart_pending()
        .then(Instant::now);
    #[cfg(windows)]
    if dxgi_recovery_state.lock().unwrap().take_fallback_pending() {
        c.set_gdi();
        log::info!("dxgi recovery exhausted, fall back to gdi");
    }
    #[cfg(windows)]
    if !scrap::codec::enable_directx_capture() && !c.is_gdi() {
        log::info!("disable dxgi with option, fall back to gdi");
        c.set_gdi();
    }
    let mut video_qos = VIDEO_QOS.lock().unwrap();
    let mut spf = video_qos.spf();
    // Not always where the stream is: see `ratio_for_creation`. `check_qos` brings an encoder
    // that follows the ratio to it before the first frame.
    let mut quality = video_qos.ratio_for_creation();
    // Fork: a hardware encoder is created for the frame rate the viewers allow, and puts out its
    // bitrate at it; `check_qos` sizes it for another while it runs.
    let mut asked_fps = video_qos.asked_fps();
    let record_incoming = config::option2bool(
        "allow-auto-record-incoming",
        &Config::get_option("allow-auto-record-incoming"),
    );
    let client_record = video_qos.record();
    drop(video_qos);
    let (mut encoder, encoder_cfg, codec_format, use_i444, recorder) = match setup_encoder(
        &c,
        sp.name(),
        quality,
        asked_fps,
        client_record,
        record_incoming,
        last_portable_service_running,
        vs.source,
        display_idx,
    ) {
        Ok(result) => result,
        Err(err) => {
            log::error!("Failed to create encoder: {err:?}, fallback to VP9");
            Encoder::set_fallback(&EncoderCfg::VPX(VpxEncoderConfig {
                width: c.width as _,
                height: c.height as _,
                quality,
                codec: VpxVideoCodecId::VP9,
                keyframe_interval: None,
            }));
            setup_encoder(
                &c,
                sp.name(),
                quality,
                asked_fps,
                client_record,
                record_incoming,
                last_portable_service_running,
                vs.source,
                display_idx,
            )?
        }
    };
    #[cfg(feature = "vram")]
    c.set_output_texture(encoder.input_texture());
    #[cfg(target_os = "android")]
    if vs.source.is_monitor() {
        if let Err(e) = check_change_scale(encoder.is_hardware()) {
            try_broadcast_display_changed(&sp, display_idx, &c, true).ok();
            bail!(e);
        }
    }
    VIDEO_QOS
        .lock()
        .unwrap()
        .store_bitrate_at(quality, encoder.bitrate());
    VIDEO_QOS
        .lock()
        .unwrap()
        .set_support_changing_quality(&sp.name(), encoder.support_changing_quality());
    let encoder_fps = encoder.set_fps(asked_fps).unwrap_or(asked_fps);
    VIDEO_QOS
        .lock()
        .unwrap()
        .set_encoder_hardware(&sp.name(), encoder.is_hardware(), encoder_fps);
    // An encoder that takes a new bitrate only by being created again is created again for the
    // network on X11 alone, where that does not take the capture down with it, and not while
    // recording, which it would cut in two.
    #[cfg(target_os = "linux")]
    {
        let restartable = is_x11() && !client_record && recorder.lock().unwrap().is_none();
        VIDEO_QOS
            .lock()
            .unwrap()
            .set_restartable(&sp.name(), restartable);
    }
    log::info!("initial quality: {quality:?}");

    if sp.is_option_true(OPTION_REFRESH) {
        sp.set_option_bool(OPTION_REFRESH, false);
    }

    let mut frame_controller = VideoFrameController::new(display_idx);

    let start = time::Instant::now();
    let mut last_check_displays = time::Instant::now();
    #[cfg(windows)]
    let mut try_gdi = 1;
    #[cfg(windows)]
    log::info!("gdi: {}", c.is_gdi());
    #[cfg(windows)]
    start_uac_elevation_check();

    #[cfg(target_os = "linux")]
    let mut would_block_count = 0u32;
    let mut yuv = Vec::new();
    let mut mid_data = Vec::new();
    let mut repeat_encode_counter = 0;
    let repeat_encode_max = 10;
    let mut encode_fail_counter = 0;
    let mut first_frame = true;
    // Set once the encoder emits its first packet; until then it may legitimately return nothing.
    let mut encoder_warmed_up = false;
    let capture_width = c.width;
    let capture_height = c.height;
    let mut static_refresh = static_refresh::StaticRefresh::new(
        vs.source,
        codec_format,
        &encoder_cfg,
        &sp,
        &recorder,
        display_idx,
        capture_width,
        capture_height,
    );
    let (mut second_instant, mut send_counter) = (Instant::now(), 0);
    // How long the loop waited for its frames to be taken since `check_qos` last read it.
    let mut held_up = Duration::ZERO;
    // Fork: whether the pace held a new picture back past its frame interval since `check_qos`
    // last read it, and whether the last round had a new picture.
    let (mut paced, mut captured_last) = (false, false);

    while sp.ok() {
        #[cfg(windows)]
        check_uac_switch(c.privacy_mode_id, c._capturer_privacy_mode_id)?;
        check_qos(
            &mut encoder,
            &mut quality,
            &mut spf,
            &mut asked_fps,
            &mut paced,
            client_record,
            &mut send_counter,
            &mut second_instant,
            &mut held_up,
            &sp.name(),
        )?;
        // Fork: whether this round has a new picture, for whether a pace says the screen changes.
        let captured_before = send_counter;
        // On macOS, a refresh here would destroy a hardware encoder that has been fed a frame but
        // not yet produced its first packet, restarting the warm-up forever. Defer until it has;
        // one that never does is still switched away from by max_fail_times in handle_one_frame.
        if sp.is_option_true(OPTION_REFRESH)
            && refresh_allowed(first_frame, encoder_warmed_up, encoder.latency_free())
        {
            if vs.source.is_monitor() {
                let _ = try_broadcast_display_changed(&sp, display_idx, &c, true);
            }
            log::info!("switch to refresh");
            bail!("SWITCH");
        }
        if codec_format != Encoder::negotiated_codec() {
            log::info!(
                "switch due to codec changed, {:?} -> {:?}",
                codec_format,
                Encoder::negotiated_codec()
            );
            bail!("SWITCH");
        }
        #[cfg(windows)]
        if last_portable_service_running != crate::portable_service::client::running() {
            log::info!("switch due to portable service running changed");
            bail!("SWITCH");
        }
        if Encoder::use_i444(&encoder_cfg) != use_i444 {
            log::info!("switch due to i444 changed");
            bail!("SWITCH");
        }
        #[cfg(all(windows, feature = "vram"))]
        if c.is_gdi() && encoder.input_texture() {
            log::info!("changed to gdi when using vram");
            VRamEncoder::set_fallback_gdi(sp.name(), true);
            bail!("SWITCH");
        }
        if vs.source.is_monitor() {
            check_privacy_mode_changed(&sp, display_idx, &c)?;
        }
        #[cfg(windows)]
        {
            if crate::platform::windows::desktop_changed()
                && !crate::portable_service::client::running()
            {
                bail!("Desktop changed");
            }
        }
        let now = time::Instant::now();
        if vs.source.is_monitor() && last_check_displays.elapsed().as_millis() > 1000 {
            last_check_displays = now;
            // This check may be redundant, but it is better to be safe.
            // The previous check in `sp.is_option_true(OPTION_REFRESH)` block may be enough.
            try_broadcast_display_changed(&sp, display_idx, &c, false)?;
        }

        // Fork: what a software encoder sent runs no further ahead of its pace than
        // `PACE_AHEAD` when the next picture is taken (`pace_wait`). Looked at again at least
        // once a second.
        let pace = pace_wait();
        if pace > super::video_qos::PACE_AHEAD {
            // A pace after a new picture: the screen changes, however few frames that leaves.
            // Counted once for that picture, however many seconds its pace takes.
            paced |= std::mem::take(&mut captured_last);
            std::thread::sleep(pace.min(Duration::from_secs(1)));
            continue;
        }

        frame_controller.reset();

        let time = now - start;
        let ms = (time.as_secs() * 1000 + time.subsec_millis() as u64) as i64;
        let res = match c.frame(spf) {
            Ok(frame) => {
                repeat_encode_counter = 0;
                if frame.valid() {
                    let screenshot_key = (vs.source, display_idx);
                    let screenshot = SCREENSHOTS.lock().unwrap().remove(&screenshot_key);
                    if let Some(mut screenshot) = screenshot {
                        let restore_vram = screenshot.restore_vram;
                        let (msg, w, h, data) = match &frame {
                            scrap::Frame::PixelBuffer(f) => match get_rgba_from_pixelbuf(f) {
                                Ok(rgba) => ("".to_owned(), f.width(), f.height(), rgba),
                                Err(e) => {
                                    let serr = e.to_string();
                                    log::error!(
                                        "Failed to convert the pix format into rgba, {}",
                                        &serr
                                    );
                                    (format!("Convert pixfmt: {}", serr), 0, 0, vec![])
                                }
                            },
                            scrap::Frame::Texture(_) => {
                                if restore_vram {
                                    // Already set one time, just ignore to break infinite loop.
                                    // Though it's unreachable, this branch is kept to avoid infinite loop.
                                    (
                                        "Please change codec and try again.".to_owned(),
                                        0,
                                        0,
                                        vec![],
                                    )
                                } else {
                                    #[cfg(all(windows, feature = "vram"))]
                                    VRamEncoder::set_not_use(sp.name(), true);
                                    screenshot.restore_vram = true;
                                    SCREENSHOTS
                                        .lock()
                                        .unwrap()
                                        .insert(screenshot_key, screenshot);
                                    _raii.try_vram = false;
                                    bail!("SWITCH");
                                }
                            }
                        };
                        std::thread::spawn(move || {
                            handle_screenshot(screenshot, msg, w, h, data);
                        });
                        if restore_vram {
                            bail!("SWITCH");
                        }
                    }

                    let frame = frame.to(encoder.yuvfmt(), &mut yuv, &mut mid_data)?;
                    static_refresh.on_frame(&frame);
                    let result = handle_one_frame(
                        display_idx,
                        &sp,
                        frame,
                        ms,
                        &mut encoder,
                        recorder.clone(),
                        &mut encode_fail_counter,
                        &mut first_frame,
                        &mut encoder_warmed_up,
                        capture_width,
                        capture_height,
                    )?;
                    static_refresh.on_frame_encoded(&result);
                    frame_controller.set_send(now, result.send_conn_ids);
                    send_counter += 1;
                }
                #[cfg(windows)]
                {
                    #[cfg(feature = "vram")]
                    if try_gdi == 1 && !c.is_gdi() {
                        VRamEncoder::set_fallback_gdi(sp.name(), false);
                    }
                    try_gdi = 0;
                }
                Ok(())
            }
            Err(err) => Err(err),
        };

        match res {
            Err(ref e) if e.kind() == WouldBlock => {
                #[cfg(windows)]
                if dxgi_recovery_started
                    .map(|started| started.elapsed() >= DXGI_RECOVERY_FRAME_GRACE)
                    .unwrap_or(true)
                {
                    if try_gdi > 0 && !c.is_gdi() {
                        if try_gdi > 3 {
                            c.set_gdi();
                            try_gdi = 0;
                            log::info!("No image, fall back to gdi");
                        }
                        try_gdi += 1;
                    }
                }
                #[cfg(target_os = "linux")]
                {
                    would_block_count += 1;
                    if !is_x11() {
                        if would_block_count >= 100 {
                            // to-do: Unknown reason for WouldBlock 100 times (seconds = 100 * 1 / fps)
                            // https://github.com/rustdesk/rustdesk/blob/63e6b2f8ab51743e77a151e2b7ff18816f5fa2fb/libs/scrap/src/common/wayland.rs#L81
                            //
                            // Do not reset the capturer for now, as it will cause the prompt to show every few minutes.
                            // https://github.com/rustdesk/rustdesk/issues/4276
                            //
                            // super::wayland::clear();
                            // bail!("Wayland capturer none 100 times, try restart capture");
                        }
                    }
                }
                if !encoder.latency_free() && yuv.len() > 0 {
                    // yun.len() > 0 means the frame is not texture.
                    if repeat_encode_counter < repeat_encode_max {
                        repeat_encode_counter += 1;
                        let send_conn_ids = handle_one_frame(
                            display_idx,
                            &sp,
                            EncodeInput::YUV(&yuv),
                            ms,
                            &mut encoder,
                            recorder.clone(),
                            &mut encode_fail_counter,
                            &mut first_frame,
                            &mut encoder_warmed_up,
                            capture_width,
                            capture_height,
                        )?
                        .send_conn_ids;
                        static_refresh.on_encoded(!send_conn_ids.is_empty());
                        frame_controller.set_send(now, send_conn_ids);
                        send_counter += 1;
                    }
                }
                static_refresh.try_encode(
                    &yuv,
                    spf,
                    quality,
                    now,
                    ms,
                    &mut encoder,
                    &mut frame_controller,
                )?;
            }
            Err(err) => {
                #[cfg(windows)]
                // The display-change check can restart capture before error handling below.
                let recovery_attempt = if !c.is_gdi() && err.kind() == ConnectionReset {
                    let mut state = dxgi_recovery_state.lock().unwrap();
                    if crate::vdisplay::layout_generation() != made_at_layout {
                        // The layout changed by our doing since this capturer was made: the access
                        // lost comes of that, not of the capture.
                        Some(state.restart_after_layout_change())
                    } else {
                        state.next_attempt()
                    }
                } else {
                    None
                };
                // This check may be redundant, but it is better to be safe.
                // The previous check in `sp.is_option_true(OPTION_REFRESH)` block may be enough.
                if vs.source.is_monitor() {
                    try_broadcast_display_changed(&sp, display_idx, &c, true)?;
                }

                #[cfg(windows)]
                if !c.is_gdi() {
                    if err.kind() == ConnectionReset {
                        if let Some(attempt) = recovery_attempt {
                            if attempt == LAYOUT_CHANGE {
                                log::debug!(
                                    "dxgi access lost through a change of our virtual displays, restart capture, not counted: {err:?}"
                                );
                            } else {
                                log::debug!(
                                    "dxgi access lost, restart capture: attempt {attempt}, error: {err:?}"
                                );
                            }
                            bail!("SWITCH");
                        }
                        log::warn!(
                            "dxgi access lost after {DXGI_RECOVERY_LIMIT} restarts in {} seconds, fall back to gdi: {err:?}",
                            DXGI_RECOVERY_WINDOW.as_secs()
                        );
                        dxgi_recovery_state.lock().unwrap().take_fallback_pending();
                    }
                    c.set_gdi();
                    log::info!("dxgi error, fall back to gdi: {:?}", err);
                    continue;
                }
                return Err(err.into());
            }
            _ => {
                #[cfg(target_os = "linux")]
                {
                    would_block_count = 0;
                }
            }
        }

        let mut fetched_conn_ids = HashSet::new();
        let timeout_millis = 3_000u64;
        let wait_begin = Instant::now();
        while wait_begin.elapsed().as_millis() < timeout_millis as _ {
            if vs.source.is_monitor() {
                check_privacy_mode_changed(&sp, display_idx, &c)?;
            }
            frame_controller.try_wait_next(&mut fetched_conn_ids, 300);
            // break if all connections have received current frame
            if fetched_conn_ids.len() >= frame_controller.send_conn_ids.len() {
                break;
            }
        }
        DISPLAY_CONN_IDS.lock().unwrap().remove(&display_idx);
        // A connection whose send is held up takes no frame: this wait is the path being full.
        held_up += wait_begin.elapsed();

        let elapsed = now.elapsed();
        // may need to enable frame(timeout)
        log::trace!("{:?} {:?}", time::Instant::now(), elapsed);
        // Fork: a new picture, not a static refresh's repeat of an old one.
        captured_last = send_counter > captured_before;
        let interval = frame_interval(spf, encoder.is_hardware());
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }

    Ok(())
}

/// Forgets what video service `name` asked of VRAM encoding while it was watched: its capture
/// falling back to GDI (`set_fallback_gdi`) and its not using VRAM (`set_not_use`). Either keeps
/// every display of the machine off VRAM encoding (`VRamEncoder::available`), H264 and H265 then
/// going from what it offers unless a RAM hardware encoder stands in. Both were dropped only by
/// that service capturing again; one nobody watches any more, such as a virtual display removed
/// at the end of its session, kept them until the service restarted. Watched again, the service
/// starts on DXGI and sets them anew where it has to. The caller works the encodings out anew.
#[cfg(all(windows, feature = "vram"))]
fn forget_vram_marks(name: &str, screenshot_key: (VideoSource, usize)) {
    VRamEncoder::set_fallback_gdi(name.to_owned(), false);
    VRamEncoder::set_not_use(name.to_owned(), false);
    // A screenshot still waiting for a pixel frame turns VRAM off for itself again when the display
    // is next captured, rather than giving up as one that already did.
    if let Some(screenshot) = SCREENSHOTS.lock().unwrap().get_mut(&screenshot_key) {
        screenshot.restore_vram = false;
    }
}

/// Forgets the VRAM marks of a capture that ends with nobody watching: the last unsubscribe may
/// have come before the capture set a mark on its way out. `Raii`, dropped right after, works the
/// encodings out anew.
#[cfg(all(windows, feature = "vram"))]
struct ForgetVramMarksIfUnwatched(GenericService, (VideoSource, usize));

#[cfg(all(windows, feature = "vram"))]
impl Drop for ForgetVramMarksIfUnwatched {
    fn drop(&mut self) {
        if !self.0.ok() {
            forget_vram_marks(&self.0.name(), self.1);
        }
    }
}

struct Raii {
    display_idx: usize,
    name: String,
    try_vram: bool,
}

impl Raii {
    fn new(display_idx: usize, name: String) -> Self {
        log::info!("new video service: {}", name);
        VIDEO_QOS.lock().unwrap().new_display(name.clone());
        Raii {
            display_idx,
            name,
            try_vram: true,
        }
    }
}

impl Drop for Raii {
    fn drop(&mut self) {
        log::info!("stop video service: {}", self.name);
        #[cfg(feature = "vram")]
        if self.try_vram {
            VRamEncoder::set_not_use(self.name.clone(), false);
        }
        #[cfg(feature = "vram")]
        Encoder::update(scrap::codec::EncodingUpdate::Check);
        VIDEO_QOS.lock().unwrap().remove_display(&self.name);
        DISPLAY_CONN_IDS.lock().unwrap().remove(&self.display_idx);
    }
}

fn setup_encoder(
    c: &CapturerInfo,
    name: String,
    quality: f32,
    fps: u32,
    client_record: bool,
    record_incoming: bool,
    last_portable_service_running: bool,
    source: VideoSource,
    display_idx: usize,
) -> ResultType<(
    Encoder,
    EncoderCfg,
    CodecFormat,
    bool,
    Arc<Mutex<Option<Recorder>>>,
)> {
    let encoder_cfg = get_encoder_config(
        &c,
        name.to_string(),
        quality,
        fps,
        client_record || record_incoming,
        last_portable_service_running,
        source,
    );
    Encoder::set_fallback(&encoder_cfg);
    let codec_format = Encoder::negotiated_codec();
    let recorder = get_recorder(record_incoming, display_idx, source == VideoSource::Camera);
    let use_i444 = Encoder::use_i444(&encoder_cfg);
    let encoder = Encoder::new(encoder_cfg.clone(), use_i444)?;
    Ok((encoder, encoder_cfg, codec_format, use_i444, recorder))
}

fn get_encoder_config(
    c: &CapturerInfo,
    _name: String,
    quality: f32,
    _fps: u32,
    record: bool,
    _portable_service: bool,
    _source: VideoSource,
) -> EncoderCfg {
    #[cfg(all(windows, feature = "vram"))]
    if _portable_service || c.is_gdi() || _source == VideoSource::Camera {
        log::info!("gdi:{}, portable:{}", c.is_gdi(), _portable_service);
        VRamEncoder::set_not_use(_name, true);
    }
    #[cfg(feature = "vram")]
    Encoder::update(scrap::codec::EncodingUpdate::Check);
    // https://www.wowza.com/community/t/the-correct-keyframe-interval-in-obs-studio/95162
    let keyframe_interval = if record { Some(240) } else { None };
    let negotiated_codec = Encoder::negotiated_codec();
    match negotiated_codec {
        CodecFormat::H264 | CodecFormat::H265 => {
            #[cfg(feature = "vram")]
            if let Some(feature) = VRamEncoder::try_get(&c.device(), negotiated_codec) {
                return EncoderCfg::VRAM(VRamEncoderConfig {
                    device: c.device(),
                    width: c.width,
                    height: c.height,
                    quality,
                    feature,
                    keyframe_interval,
                    fps: _fps,
                });
            }
            #[cfg(feature = "hwcodec")]
            if let Some(hw) = HwRamEncoder::try_get(negotiated_codec) {
                return EncoderCfg::HWRAM(HwRamEncoderConfig {
                    name: hw.name,
                    mc_name: hw.mc_name,
                    width: c.width,
                    height: c.height,
                    quality,
                    keyframe_interval,
                    fps: _fps,
                });
            }
            EncoderCfg::VPX(VpxEncoderConfig {
                width: c.width as _,
                height: c.height as _,
                quality,
                codec: VpxVideoCodecId::VP9,
                keyframe_interval,
            })
        }
        format @ (CodecFormat::VP8 | CodecFormat::VP9) => EncoderCfg::VPX(VpxEncoderConfig {
            width: c.width as _,
            height: c.height as _,
            quality,
            codec: if format == CodecFormat::VP8 {
                VpxVideoCodecId::VP8
            } else {
                VpxVideoCodecId::VP9
            },
            keyframe_interval,
        }),
        CodecFormat::AV1 => EncoderCfg::AOM(AomEncoderConfig {
            width: c.width as _,
            height: c.height as _,
            quality,
            keyframe_interval,
        }),
        _ => EncoderCfg::VPX(VpxEncoderConfig {
            width: c.width as _,
            height: c.height as _,
            quality,
            codec: VpxVideoCodecId::VP9,
            keyframe_interval,
        }),
    }
}

fn get_recorder(
    record_incoming: bool,
    display_idx: usize,
    camera: bool,
) -> Arc<Mutex<Option<Recorder>>> {
    #[cfg(windows)]
    let root = crate::platform::is_root();
    #[cfg(not(windows))]
    let root = false;
    let recorder = if record_incoming {
        use crate::hbbs_http::record_upload;

        let tx = if record_upload::is_enable() {
            let (tx, rx) = std::sync::mpsc::channel();
            record_upload::run(rx);
            Some(tx)
        } else {
            None
        };
        Recorder::new(RecorderContext {
            server: true,
            id: Config::get_id(),
            dir: crate::ui_interface::video_save_directory(root),
            display_idx,
            camera,
            tx,
        })
        .map_or(Default::default(), |r| Arc::new(Mutex::new(Some(r))))
    } else {
        Default::default()
    };

    recorder
}

#[cfg(target_os = "android")]
fn check_change_scale(hardware: bool) -> ResultType<()> {
    use base::config::keys::OPTION_ENABLE_ANDROID_SOFTWARE_ENCODING_HALF_SCALE as SCALE_SOFT;

    // isStart flag is set at the end of startCapture() in Android, wait it to be set.
    let n = 60; // 3s
    for i in 0..n {
        if scrap::is_start() == Some(true) {
            log::info!("start flag is set");
            break;
        }
        log::info!("wait for start, {i}");
        std::thread::sleep(Duration::from_millis(50));
        if i == n - 1 {
            log::error!("wait for start timeout");
        }
    }
    let screen_size = scrap::screen_size();
    let scale_soft = hbb_common::config::option2bool(SCALE_SOFT, &Config::get_option(SCALE_SOFT));
    let half_scale = !hardware && scale_soft;
    log::info!("hardware: {hardware}, scale_soft: {scale_soft}, screen_size: {screen_size:?}",);
    scrap::android::call_main_service_set_by_name(
        "half_scale",
        Some(half_scale.to_string().as_str()),
        None,
    )
    .ok();
    let old_scale = screen_size.2;
    let new_scale = scrap::screen_size().2;
    log::info!("old_scale: {old_scale}, new_scale: {new_scale}");
    if old_scale != new_scale {
        log::info!("switch due to scale changed, {old_scale} -> {new_scale}");
        // switch is not a must, but it is better to do so.
        bail!("SWITCH");
    }
    Ok(())
}

fn check_privacy_mode_changed(
    sp: &GenericService,
    display_idx: usize,
    ci: &CapturerInfo,
) -> ResultType<()> {
    let privacy_mode_id_2 = get_privacy_mode_conn_id().unwrap_or(INVALID_PRIVACY_MODE_CONN_ID);
    if ci.privacy_mode_id != privacy_mode_id_2 {
        if privacy_mode_id_2 != INVALID_PRIVACY_MODE_CONN_ID {
            let msg_out = crate::common::make_privacy_mode_msg(
                back_notification::PrivacyModeState::PrvOnByOther,
                "".to_owned(),
            );
            sp.send_to_others(msg_out, privacy_mode_id_2);
        }
        log::info!("switch due to privacy mode changed");
        try_broadcast_display_changed(&sp, display_idx, ci, true).ok();
        bail!("SWITCH");
    }
    Ok(())
}

#[inline]
fn refresh_allowed(unfed: bool, warmed_up: bool, latency_free: bool) -> bool {
    // Only VideoToolbox has been seen to need this; other platforms keep refreshing as before.
    !cfg!(target_os = "macos") || unfed || warmed_up || latency_free
}

#[inline]
fn handle_one_frame(
    display: usize,
    sp: &GenericService,
    frame: EncodeInput,
    ms: i64,
    encoder: &mut Encoder,
    recorder: Arc<Mutex<Option<Recorder>>>,
    encode_fail_counter: &mut usize,
    first_frame: &mut bool,
    encoder_warmed_up: &mut bool,
    width: usize,
    height: usize,
) -> ResultType<static_refresh::FrameEncodeResult> {
    sp.snapshot(|sps| {
        // so that new sub and old sub share the same encoder after switch
        if sps.has_subscribes() {
            log::info!("switch due to new subscriber");
            bail!("SWITCH");
        }
        Ok(())
    })?;

    let mut send_conn_ids: HashSet<i32> = Default::default();
    let mut vpx_no_output = false;
    let first = *first_frame;
    *first_frame = false;
    match timed_encode(encoder, frame, ms) {
        Ok(mut vf) => {
            *encode_fail_counter = 0;
            *encoder_warmed_up = true;
            // The hardware codecs work, should they have been put back after a failure.
            #[cfg(all(feature = "hwcodec", not(any(target_os = "android", target_os = "ios"))))]
            if encoder.is_hardware() {
                scrap::hwcodec::HwCodecConfig::worked();
            }
            vf.display = display as _;
            let mut msg = Message::new();
            msg.set_video_frame(vf);
            recorder
                .lock()
                .unwrap()
                .as_mut()
                .map(|r| r.write_message(&msg, width, height));
            count_sent(&msg);
            send_conn_ids = sp.send_video_frame(msg);
        }
        Err(e) => {
            vpx_no_output = e.is::<scrap::VpxNoOutput>();
            // Fork: a frame VP8/VP9's rate control left out is no failure. It does so for every
            // other frame and more on a narrow path, now that its buffer is a second.
            if vpx_no_output {
                hbb_common::throttled_log!(
                    Duration::from_secs(60),
                    debug,
                    "frames left out by the rate control: {e:?}"
                );
            } else {
                *encode_fail_counter += 1;
                // Encoding errors are not frequent except on Android
                if !cfg!(target_os = "android") {
                    log::error!("encode fail: {e:?}, times: {}", *encode_fail_counter,);
                }
            }
            let max_fail_times = if cfg!(target_os = "android") && encoder.is_hardware() {
                9
            } else {
                3
            };
            let repeat = !encoder.latency_free();
            // repeat encoders can reach max_fail_times on the first frame
            if (first && !repeat) || *encode_fail_counter >= max_fail_times {
                *encode_fail_counter = 0;
                if encoder.is_hardware() {
                    encoder.disable();
                    log::error!("switch due to encoding fails, first frame: {first}, error: {e:?}");
                    bail!("SWITCH");
                }
            }
            match e.to_string().as_str() {
                scrap::codec::ENCODE_NEED_SWITCH => {
                    encoder.disable();
                    log::error!("switch due to encoder need switch");
                    bail!("SWITCH");
                }
                _ => {}
            }
        }
    }
    Ok(static_refresh::FrameEncodeResult {
        send_conn_ids,
        vpx_no_output,
    })
}

#[inline]
pub fn refresh() {
    #[cfg(target_os = "android")]
    Display::refresh_size();
}

#[cfg(windows)]
fn start_uac_elevation_check() {
    static START: Once = Once::new();
    START.call_once(|| {
        if !crate::platform::is_installed() && !crate::platform::is_root() {
            std::thread::spawn(|| loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if let Ok(uac) = is_process_consent_running() {
                    *IS_UAC_RUNNING.lock().unwrap() = uac;
                }
                if !crate::platform::is_elevated(None).unwrap_or(false) {
                    if let Ok(elevated) = crate::platform::is_foreground_window_elevated() {
                        *IS_FOREGROUND_WINDOW_ELEVATED.lock().unwrap() = elevated;
                    }
                }
            });
        }
    });
}

#[inline]
fn try_broadcast_display_changed(
    sp: &GenericService,
    display_idx: usize,
    cap: &CapturerInfo,
    refresh: bool,
) -> ResultType<()> {
    if refresh {
        // Get display information immediately.
        crate::display_service::check_displays_changed().ok();
    }
    if let Some(display) = check_display_changed(
        cap.ndisplay,
        cap.current,
        (cap.origin.0, cap.origin.1, cap.width, cap.height),
    ) {
        log::info!("Display {} changed", display);
        if let Some(msg_out) =
            make_display_changed_msg(display_idx, Some(display), VideoSource::Monitor)
        {
            let msg_out = Arc::new(msg_out);
            sp.send_shared(msg_out.clone());
            // switch display may occur before the first video frame, add snapshot to send to new subscribers
            sp.snapshot(move |sps| {
                sps.send_shared(msg_out.clone());
                Ok(())
            })?;
            bail!("SWITCH");
        }
    }
    Ok(())
}

pub fn make_display_changed_msg(
    display_idx: usize,
    opt_display: Option<DisplayInfo>,
    source: VideoSource,
) -> Option<Message> {
    let display = match opt_display {
        Some(d) => d,
        None => match source {
            VideoSource::Monitor => display_service::get_display_info(display_idx)?,
            VideoSource::Camera => camera::Cameras::get_sync_cameras()
                .get(display_idx)?
                .clone(),
        },
    };
    let mut misc = Misc::new();
    misc.set_switch_display(SwitchDisplay {
        display: display_idx as _,
        x: display.x,
        y: display.y,
        width: display.width,
        height: display.height,
        cursor_embedded: match source {
            // The display's own, or the client takes this one for it and draws a second cursor.
            VideoSource::Monitor => {
                display_service::capture_cursor_embedded() || display.cursor_embedded
            }
            VideoSource::Camera => false,
        },
        #[cfg(not(target_os = "android"))]
        resolutions: Some(SupportedResolutions {
            resolutions: match source {
                VideoSource::Monitor => {
                    if display.name.is_empty() {
                        vec![]
                    } else {
                        crate::platform::resolutions(&display.name)
                    }
                }
                VideoSource::Camera => camera::Cameras::get_camera_resolution(display_idx)
                    .ok()
                    .into_iter()
                    .collect(),
            },
            ..SupportedResolutions::default()
        })
        .into(),
        original_resolution: display.original_resolution,
        ..Default::default()
    });
    let mut msg_out = Message::new();
    msg_out.set_misc(misc);
    Some(msg_out)
}

fn check_qos(
    encoder: &mut Encoder,
    ratio: &mut f32,
    spf: &mut Duration,
    asked_fps: &mut u32,
    paced: &mut bool,
    client_record: bool,
    send_counter: &mut usize,
    second_instant: &mut Instant,
    held_up: &mut Duration,
    name: &str,
) -> ResultType<()> {
    let mut video_qos = VIDEO_QOS.lock().unwrap();
    *spf = video_qos.spf();
    // Fork: a hardware encoder keeps to its bitrate, and is not paced.
    set_pace(video_qos.pace_kbps().filter(|_| !encoder.is_hardware()));
    if *ratio != video_qos.ratio() {
        *ratio = video_qos.ratio();
        if encoder.support_changing_quality() {
            allow_err!(encoder.set_quality(*ratio));
            video_qos.store_bitrate(encoder.bitrate());
        } else {
            // Now only vaapi doesn't support changing quality
            // Where the controller creates such an encoder again itself, it says when
            // (`take_restart_request` below): not for every ratio it passes through.
            if !video_qos.in_vbr_state()
                && !video_qos.restart_only()
                && !video_qos.latest_quality().is_custom()
            {
                log::info!("switch to change quality");
                bail!("SWITCH");
            }
        }
    }
    // Such an encoder is also created again when the controller asks for it: for the network,
    // or for a custom quality the viewer moved. Asked whether or not the ratio moved since the
    // last frame: it may have moved before that.
    if !encoder.support_changing_quality() && video_qos.take_restart_request(name) {
        log::info!("switch to change quality, asked by qos");
        bail!("SWITCH");
    }
    if client_record != video_qos.record() {
        log::info!("switch due to record changed");
        bail!("SWITCH");
    }
    if second_instant.elapsed() > Duration::from_secs(1) {
        let elapsed = second_instant.elapsed();
        *second_instant = Instant::now();
        // Fork: the viewers allow another frame rate. A hardware encoder that takes a new bitrate
        // while it runs is sized for it, by the second; one that does not is created again for
        // it by the controller, which is told what it puts out its bitrate at either way.
        let fps = video_qos.asked_fps();
        if fps != *asked_fps {
            if !encoder.is_hardware() {
                *asked_fps = fps;
            } else {
                match encoder.set_fps(fps) {
                    Ok(sized) => {
                        *asked_fps = fps;
                        video_qos.set_encoder_hardware(name, true, sized);
                    }
                    // Tried again a second later.
                    Err(err) => log::warn!("failed to size the encoder for {fps} fps: {err:?}"),
                }
            }
        }
        video_qos.update_display_rates(
            &name,
            SENT_BYTES.with(|bytes| bytes.replace(0)),
            SENT_FRAMES.with(|frames| frames.replace(0)),
            std::mem::take(held_up),
            elapsed,
        );
        let frames = VideoQoS::frames_or_paced(*send_counter, std::mem::take(paced));
        video_qos.update_display_data(&name, frames);
        *send_counter = 0;
    }
    drop(video_qos);
    Ok(())
}

pub fn set_take_screenshot(source: VideoSource, display_idx: usize, sid: String, tx: Sender) {
    SCREENSHOTS.lock().unwrap().insert(
        (source, display_idx),
        Screenshot {
            sid,
            tx,
            restore_vram: false,
        },
    );
}

// We need to this function, because the `stride` may be larger than `width * 4`.
fn get_rgba_from_pixelbuf<'a>(pixbuf: &scrap::PixelBuffer<'a>) -> ResultType<Vec<u8>> {
    let w = pixbuf.width();
    let h = pixbuf.height();
    let stride = pixbuf.stride();
    let Some(s) = stride.get(0) else {
        bail!("Invalid pixel buf stride.")
    };

    if *s == w * 4 {
        let mut rgba = vec![];
        scrap::convert(pixbuf, scrap::Pixfmt::RGBA, &mut rgba)?;
        Ok(rgba)
    } else {
        let bgra = pixbuf.data();
        let mut bit_flipped = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let i = s * y + 4 * x;
                bit_flipped.extend_from_slice(&[bgra[i + 2], bgra[i + 1], bgra[i], bgra[i + 3]]);
            }
        }
        Ok(bit_flipped)
    }
}

fn handle_screenshot(screenshot: Screenshot, msg: String, w: usize, h: usize, data: Vec<u8>) {
    let mut response = ScreenshotResponse::new();
    response.sid = screenshot.sid;
    if msg.is_empty() {
        if data.is_empty() {
            response.msg = "Failed to take screenshot, please try again later.".to_owned();
        } else {
            fn encode_png(width: usize, height: usize, rgba: Vec<u8>) -> ResultType<Vec<u8>> {
                let mut png = Vec::new();
                let mut encoder =
                    repng::Options::smallest(width as _, height as _).build(&mut png)?;
                encoder.write(&rgba)?;
                encoder.finish()?;
                Ok(png)
            }
            match encode_png(w as _, h as _, data) {
                Ok(png) => {
                    response.data = png.into();
                }
                Err(e) => {
                    response.msg = format!("Error encoding png: {}", e);
                }
            }
        }
    } else {
        response.msg = msg;
    }
    let mut msg_out = Message::new();
    msg_out.set_screenshot_response(response);
    if let Err(e) = screenshot
        .tx
        .send((hbb_common::tokio::time::Instant::now(), Arc::new(msg_out)))
    {
        log::error!("Failed to send screenshot, {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::refresh_allowed;

    #[test]
    fn refresh_waits_for_hw_encoder_warmup() {
        // Deferred on macOS only; everywhere else a refresh is always allowed.
        assert_eq!(
            refresh_allowed(false, false, false),
            !cfg!(target_os = "macos")
        );
        assert!(refresh_allowed(false, true, false));
        assert!(refresh_allowed(false, false, true));
        // Nothing fed yet: no warm-up to lose, and a refresh can restart a stalled capturer.
        assert!(refresh_allowed(true, false, false));
    }

    #[test]
    fn a_software_encoder_waits_for_what_it_sent_over_its_pace() {
        use super::{pace_wait, set_pace, PACE_AT, PACE_BITS};
        use std::time::{Duration, Instant};
        set_pace(Some(1000.0));
        // Nothing sent: nothing to wait for.
        assert_eq!(pace_wait(), Duration::ZERO);
        // A frame of 500 kbit at a pace of 1000 kbps: the next one waits half a second, less
        // what has passed since.
        PACE_BITS.with(|bits| bits.set(500_000.0));
        let wait = pace_wait();
        assert!(wait <= Duration::from_millis(500), "{wait:?}");
        assert!(wait > Duration::from_millis(450), "{wait:?}");
        // Paid off as time passes.
        PACE_AT.with(|at| at.set(Some(Instant::now() - Duration::from_millis(300))));
        assert!(pace_wait() <= Duration::from_millis(200));
        // Time with nothing owed is no credit for the frames after it.
        PACE_BITS.with(|bits| bits.set(0.0));
        PACE_AT.with(|at| at.set(Some(Instant::now() - Duration::from_secs(5))));
        assert_eq!(pace_wait(), Duration::ZERO);
        PACE_BITS.with(|bits| bits.set(300_000.0));
        assert!(pace_wait() > Duration::from_millis(250));
        // No pace: nothing waits, and nothing is owed when one comes.
        set_pace(None);
        assert_eq!(pace_wait(), Duration::ZERO);
        assert_eq!(PACE_BITS.with(|bits| bits.get()), 0.0);
    }

    #[test]
    fn a_slow_software_encoder_is_held_back_only_above_thirty_frames() {
        use super::{frame_interval, note_encode_time, ENCODE_TIME, SLOWEST_PACE};
        use std::time::Duration;
        let (sixty, thirty) = (Duration::from_micros(16_667), Duration::from_micros(33_333));
        // An encoder that took these milliseconds for its frames, one after the other.
        let after = |frames: &[u64]| {
            ENCODE_TIME.with(|time| time.set(None));
            for ms in frames {
                note_encode_time(Duration::from_millis(*ms));
            }
        };
        // Nothing encoded yet: the pace asked for.
        after(&[]);
        assert_eq!(frame_interval(sixty, false), sixty);
        // 4 ms a frame leaves sixty frames a second alone.
        after(&[4; 20]);
        assert_eq!(frame_interval(sixty, false), sixty);
        // 12 ms a frame: twice that between frames, which is 41 a second.
        after(&[12; 20]);
        assert_eq!(frame_interval(sixty, false), Duration::from_millis(24));
        // 40 ms a frame: no slower than thirty a second for it, where the loop then runs as
        // fast as the encoder lets it, as it always did.
        after(&[40; 20]);
        assert_eq!(frame_interval(sixty, false), SLOWEST_PACE);
        // At thirty frames a second or fewer nothing changes, however slow the encoder.
        assert_eq!(frame_interval(thirty, false), thirty);
        assert_eq!(
            frame_interval(Duration::from_millis(100), false),
            Duration::from_millis(100)
        );
        // A hardware encoder is never held back.
        assert_eq!(frame_interval(sixty, true), sixty);
        // The quick frames of a still screen do not hide what the frames of a moving one take.
        let mut frames = vec![12; 20];
        frames.extend([1; 5]);
        after(&frames);
        assert!(frame_interval(sixty, false) > Duration::from_millis(18));
        ENCODE_TIME.with(|time| time.set(None));
    }
}
