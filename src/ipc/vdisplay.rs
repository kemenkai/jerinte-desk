// The `_vdisplay` channel: the session's `--server` asks the root service for connectors to light
// virtual displays on (src/vdisplay/x11). A socket of its own next to `_service`, as `_drm` has:
// any local user can connect to it, so every connection is authorized here, the way the uinput
// channels authorize theirs.

use super::*;
use std::os::unix::{
    io::AsRawFd,
    net::{UnixListener, UnixStream},
};
use std::sync::{atomic::AtomicUsize, Arc};

// One `--server` per session needs one connection.
const MAX_CONNS: usize = 4;

fn vdisplay_ipc_path() -> String {
    let service_path = Config::ipc_path("_service");
    let dir = std::path::Path::new(&service_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/tmp"));
    dir.join("ipc_vdisplay").to_string_lossy().into_owned()
}

/// The root service's end, for the life of the service: each authorized connection is served by
/// `serve(stream, client, uid, pid)` on a thread of its own, `client` numbering the connections,
/// `uid` the user on the other end and `pid` its process, as the socket says.
pub(crate) fn listen_vdisplay(
    serve: impl Fn(UnixStream, u64, u32, u32) + Send + Sync + 'static,
) -> ResultType<()> {
    let path = vdisplay_ipc_path();
    ensure_secure_ipc_parent_dir(&path, "_service")?;
    if let Err(err) = remove_ipc_entry_via_secure_parent_fd(&path) {
        log::warn!("vdisplay: could not clear a stale entry at {}: {}", &path, err);
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
    log::info!("Started vdisplay ipc server at path: {}", &path);
    let serve = Arc::new(serve);
    let conns = Arc::new(AtomicUsize::new(0));
    let mut next_client = 0u64;
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(err) => {
                log::error!("vdisplay: accept failed: {}", err);
                continue;
            }
        };
        // Debug, not warn: anyone may knock, and each knock must not be a line in the log.
        let Some((uid, pid)) = authorized(&stream) else {
            continue;
        };
        if conns.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
            conns.fetch_sub(1, Ordering::SeqCst);
            log::debug!("vdisplay: too many connections; refusing one");
            continue;
        }
        next_client += 1;
        let (serve, held, client) = (serve.clone(), conns.clone(), next_client);
        let spawned = std::thread::Builder::new()
            .name("vdisplay-conn".into())
            .spawn(move || {
                serve(stream, client, uid, pid);
                held.fetch_sub(1, Ordering::SeqCst);
            });
        if let Err(err) = spawned {
            // The thread that would have given the place back never ran.
            conns.fetch_sub(1, Ordering::SeqCst);
            log::error!("vdisplay: could not serve a connection: {}", err);
        }
    }
    Ok(())
}

/// The session's end: a connection to the root service, and to nothing else.
pub(crate) fn connect_vdisplay(timeout: std::time::Duration) -> ResultType<UnixStream> {
    let stream = UnixStream::connect(vdisplay_ipc_path())?;
    if peer_uid_from_fd(stream.as_raw_fd()) != Some(0) {
        bail!("vdisplay: the service end is not root");
    }
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(stream)
}

// The session's user (or root), running this very program: its uid and pid.
fn authorized(stream: &UnixStream) -> Option<(u32, u32)> {
    let fd = stream.as_raw_fd();
    let peer_uid = peer_uid_from_fd(fd);
    let active_uid = crate::platform::linux::get_active_userid_fresh()
        .trim()
        .parse::<u32>()
        .ok();
    let Some(uid) = peer_uid.filter(|uid| is_allowed_service_peer_uid(*uid, active_uid)) else {
        log::debug!(
            "vdisplay: refused a connection from uid {:?} (active {:?})",
            peer_uid,
            active_uid
        );
        return None;
    };
    if let Err(err) = ensure_peer_executable_matches_current_by_fd(fd, "_vdisplay") {
        log::debug!("vdisplay: refused a connection from another program: {}", err);
        return None;
    }
    Some((uid, peer_pid_from_fd(fd)?))
}
