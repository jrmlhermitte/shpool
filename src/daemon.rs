use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::{time, thread, process, net};

use anyhow::{anyhow, Context};
use log::{info, error};
use serde_derive::Deserialize;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use crossbeam::channel;

use super::consts;
use super::protocol;

#[derive(Deserialize)]
struct Config {
    // TODO(ethan): implement keepalive support
    // keepalive_secs: Option<usize>,
}


struct ShellSession {
    started_at: time::SystemTime,
    inner: Arc<Mutex<ShellSessionInner>>,
}

/// ShellSessionInner contains values that the pipe thread needs to be
/// able to mutate and fully control.
struct ShellSessionInner {
    shell_proc: process::Child,
    client_stream: UnixStream,
}

pub fn run(config_file: String, socket: PathBuf) -> anyhow::Result<()> {
    let config_str = std::fs::read_to_string(config_file).context("reading config toml")?;
    let _config: Config = toml::from_str(&config_str).context("parsing config file")?;


    let mut daemon = Daemon {
        shells: HashMap::new(),
    };

    let teardown_socket = socket.clone();
    ctrlc::set_handler(move || {
        info!("ctrlc handler: cleaning up socket");
        if let Err(e)= std::fs::remove_file(teardown_socket.clone()).context("cleaning up socket") {
            error!("error cleaning up socket file: {}", e);
        }

        info!("ctrlc handler: exiting");
        std::process::exit(128 + 2 /* default SIGINT exit code */);
    }).context("registering ctrlc handler")?;

    info!("listening on socket {:?}", socket);
    let listener = UnixListener::bind(&socket).context("binding to socket")?;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(err) = daemon.handle_conn(stream) {
                    error!("handling new connection: {:?}", err)
                }
            }
            Err(err) => {
                error!("accepting stream: {:?}", err);
            }
        }
    }

    std::fs::remove_file(socket).context("cleaning up socket after no more incoming")?;

    Ok(())
}

struct Daemon {
    // a map from shell session names to session descriptors
    shells: HashMap<String, ShellSession>,
}

impl Daemon {
    fn handle_conn(&mut self, mut stream: UnixStream) -> anyhow::Result<()> {
        info!("handling inbound connection");
        // We want to avoid timing out while blocking the main thread.
        stream.set_read_timeout(Some(consts::SOCK_STREAM_TIMEOUT))
            .context("setting read timout on inbound session")?;

        let header = parse_connect_header(&mut stream)?;

        // Unset the read timeout before we pass things off to a
        // worker thread because it is perfectly fine for there to
        // be no new data for long periods of time when the users
        // is connected to a shell session.
        stream.set_read_timeout(None)
            .context("unsetting read timout on inbound session")?;

        match header {
            protocol::ConnectHeader::Attach(h) => self.handle_attach(stream, h),
            protocol::ConnectHeader::List => self.handle_list(stream),
        }
    }

    fn handle_attach(&mut self, mut stream: UnixStream, header: protocol::AttachHeader) -> anyhow::Result<()> {
        info!("handling attach header={:?}", header);

        let status: protocol::AttachStatus;
        if let Some(session) = self.shells.get(&header.name) {
            if let Ok(mut inner) = session.inner.try_lock() {
                inner.client_stream = stream;
                
                write_reply(&mut inner.client_stream, protocol::AttachReplyHeader{
                    status: protocol::AttachStatus::Attached,
                })?;

                status = protocol::AttachStatus::Attached;
                // fallthrough to bidi streaming
            } else {
                // The stream is busy, so we just inform the client and close the stream.
                write_reply(&mut stream, protocol::AttachReplyHeader{
                    status: protocol::AttachStatus::Busy,
                })?;
                stream.shutdown(net::Shutdown::Both).context("closing stream")?;
                return Ok(())
            }
        } else {
            let inner = spawn_subshell(stream, &header)?;

            self.shells.insert(header.name.clone(), ShellSession {
                started_at: time::SystemTime::now(),
                inner: Arc::new(Mutex::new(inner)),
            });

            status = protocol::AttachStatus::Created;
            // fallthrough to bidi streaming
        }

        // the nested "if let" buisness is to please the borrow checker
        if let Some(session) = self.shells.get(&header.name) {
            let inner = Arc::clone(&session.inner);
            thread::spawn(move || {
                if let Ok(mut inner) = inner.lock() {
                    let reply_status = write_reply(&mut inner.client_stream, protocol::AttachReplyHeader{
                        status,
                    });
                    if let Err(e) = reply_status {
                        error!("error writing reply status: {}", e);
                    }

                    if let Err(e) = bidi_stream(&mut inner) {
                        error!("error shuffling bytes: {}", e);
                    }
                } else {
                    error!("inernal error: failed to lock just created mutex");
                }
            });
        } else {
            error!("inernal error: failed to fetch just inserted session");
        }

        Ok(())
    }

    fn handle_list(&self, mut stream: UnixStream) -> anyhow::Result<()> {
        info!("responding to list request");

        let sessions: anyhow::Result<Vec<protocol::Session>> = self.shells
            .iter()
            .map(|(k, v)| {
                Ok(protocol::Session {
                    name: k.to_string(),
                    started_at_unix_ms: v.started_at
                                            .duration_since(time::UNIX_EPOCH)?
                                            .as_millis() as i64,
                })
            }).collect();
        let sessions = sessions.context("collecting running session metadata")?;

        write_reply(&mut stream, protocol::ListReply { sessions })?;

        Ok(())
    }
}

fn bidi_stream(inner: &mut ShellSessionInner) -> anyhow::Result<()> {
    // set timeouts so we can wake up to handle cancelation correctly
    inner.client_stream.set_read_timeout(Some(consts::PIPE_POLL_DURATION))?;
    inner.client_stream.set_write_timeout(Some(consts::PIPE_POLL_DURATION))?;
    
    // clone the client stream handle so it won't be borrowed in two
    // closures
    let mut client_read_stream = inner.client_stream.try_clone().context("cloning client read stream")?;

    // create a channel so we can make sure both worker threads exit
    // if one of them does
    let (stop_tx, stop_rx) = channel::bounded(1);

    thread::scope(|s| -> anyhow::Result<()> {
        // client -> shell
        let client_to_shell_h = s.spawn(|| -> anyhow::Result<()> {
            let mut buf: Vec<u8> = vec![0; consts::BUF_SIZE];
            let mut stdin = inner.shell_proc.stdin.as_ref().ok_or(anyhow!("missing stdin"))?;
            loop {
                if let Ok(_) = stop_rx.try_recv() {
                    return Ok(())
                }

                let len = match client_read_stream.read(&mut buf) {
                    Ok(l) => l,
                    Err(e) => {
                        if e.kind() == std::io::ErrorKind::WouldBlock {
                            thread::sleep(consts::PIPE_POLL_DURATION);
                            continue;
                        }
                        return Err(e).context("reading client chunk");
                    }
                };
                
                let mut to_write = &buf[0..len];
                while to_write.len() > 0 {
                    if let Ok(_) = stop_rx.try_recv() {
                        return Ok(())
                    }

                    // TODO(ethan): can I instead just set stdin to nonblocking
                    //              mode and just look for WouldBlock errors as I
                    //              do above? That feels simpler and more portable
                    //              than using a custom select call like this.

                    // We need to select on the stdin fd before actually doing
                    // the write because we can't block this thread without
                    // waking up to check if we are supposed to bail every so
                    // often, and we can't just set a timeout on the stdin
                    // handle unfortunately. This means that shpool only
                    // works on unix systems.
                    let mut stdin_set = nix::sys::select::FdSet::new();
                    stdin_set.insert(stdin.as_raw_fd());
                    let mut poll_dur = consts::PIPE_POLL_DURATION_TIMEVAL.clone();
                    let nready = nix::sys::select::select(
                        None, // nix will auto calculate nfds for us
                        None,
                        Some(&mut stdin_set),
                        None,
                        Some(&mut poll_dur),
                    ).context("selecting on stdin")?;
                    if nready == 0 || !stdin_set.contains(stdin.as_raw_fd()) {
                        // we got a timeout, so it is time to check stop_rx again
                        continue;
                    }

                    // TODO(ethan): check for timeout error and continue here?
                    let nwritten = stdin.write(&to_write).context("writing client chunk")?;
                    to_write = &to_write[nwritten..];
                }

                stdin.flush().context("flushing stdin")?;
            }
        });

        // shell -> client
        let shell_to_client_h = s.spawn(|| -> anyhow::Result<()> {
            let mut buf: Vec<u8> = vec![0; consts::BUF_SIZE];
            let stdout = inner.shell_proc.stdout.as_mut()
                    .ok_or(anyhow!("missing stdout"))?;
            loop {
                if let Ok(_) = stop_rx.try_recv() {
                    return Ok(())
                }

                let mut stdout_set = nix::sys::select::FdSet::new();
                stdout_set.insert(stdout.as_raw_fd());
                let mut poll_dur = consts::PIPE_POLL_DURATION_TIMEVAL.clone();
                let nready = nix::sys::select::select(
                    None,
                    Some(&mut stdout_set),
                    None,
                    None,
                    Some(&mut poll_dur),
                ).context("selecting on stdout")?;
                if nready == 0 || !stdout_set.contains(stdout.as_raw_fd()) {
                    continue;
                }

                // TODO(ethan): check for timeout error and continue here?
                let len = stdout.read(&mut buf).context("reading shell chunk")?;

                let mut to_write = &buf[0..len];
                while to_write.len() > 0 {
                    if let Ok(_) = stop_rx.try_recv() {
                        return Ok(())
                    }

                    // TODO(ethan): check for timeout error and continue here?
                    let nwritten = inner.client_stream
                        .write(&to_write).context("writing shell chunk")?;
                    to_write = &to_write[nwritten..];
                }

                // flush immediately
                inner.client_stream.flush().context("flushing client stream")?;
            }
        });

        loop {
            if client_to_shell_h.is_finished() || shell_to_client_h.is_finished() {
                stop_tx.send(true).context("sending stop msg")?;
                break;
            }
            thread::sleep(consts::JOIN_POLL_DURATION);
        }
        match client_to_shell_h.join() {
            Ok(v) => v?,
            Err(panic_err) => std::panic::resume_unwind(panic_err),
        }
        match shell_to_client_h.join() {
            Ok(v) => v?,
            Err(panic_err) => std::panic::resume_unwind(panic_err),
        }

        Ok(())
    })?;

    Ok(())
} 

fn spawn_subshell(
    client_stream: UnixStream,
    header: &protocol::AttachHeader,
) -> anyhow::Result<ShellSessionInner> {
    let user_info = user_info()?;
    info!("user_info={:?}", user_info);
    // TODO(ethan): what about stderr? I need to pass that back to the client
    //              as well. Maybe I should make a little framing protocol and
    //              have some stdin frames as well as stdout frames.
    let child = process::Command::new(user_info.default_shell)
        .stdin(process::Stdio::piped())
        .stdout(process::Stdio::piped())
        .current_dir(user_info.home_dir)
        // The env should mostly be set up by the shell sourcing
        // rc files and whatnot, so we will start things off with
        // an environment that is blank except for a little marker
        // environment variable that people can hook into for scripts
        // and whatnot.
        .env_clear().env("SHPOOL_SESSION_NAME", &header.name)
        .arg("-i") // TODO(ethan): HACK: find some way to indicate this in a
                   //              shell agnostic way
        .arg("-l") // TODO(ethan): HACK: we should be using a pty rather than forcing
                   //              a login shell with a shell-specific flag.
        .spawn()
        .context("spawning subshell")?;

    Ok(ShellSessionInner {
        shell_proc: child,
        client_stream,
    })
}

#[derive(Debug)]
struct UserInfo {
    default_shell: String,
    home_dir: String,
}

fn user_info() -> anyhow::Result<UserInfo> {
    let out = process::Command::new("/bin/sh")
        .arg("-c")
        .arg("cd ; echo \"$SHELL|$PWD\"")
        .output()
        .context("spawning subshell to determine default shell")?;
    if !out.status.success() {
        return Err(anyhow!("bad status checking for default shell: {}", out.status));
    }
    if out.stderr.len() != 0 {
        return Err(anyhow!("unexpected stderr when checking for default shell: {}",
                           String::from_utf8_lossy(&out.stderr)));
    }

    let parts = String::from_utf8(out.stdout.clone())
        .context("parsing default shell as utf8")?
        .trim().split("|").map(String::from).collect::<Vec<String>>();
    if parts.len() != 2 {
        return Err(anyhow!("could not parse output: '{}'", 
                           String::from_utf8_lossy(&out.stdout)));
    }
    Ok(UserInfo {
        default_shell: parts[0].clone(),
        home_dir: parts[1].clone(),
    })
}

fn parse_connect_header(stream: &mut UnixStream) -> anyhow::Result<protocol::ConnectHeader> {
    let length_prefix = stream.read_u32::<LittleEndian>()
        .context("reading header length prefix")?;
    let mut buf: Vec<u8> = vec![0; length_prefix as usize];
    stream.read_exact(&mut buf).context("reading header buf")?;

    let header: protocol::ConnectHeader =
        rmp_serde::from_slice(&buf).context("parsing header")?;
    Ok(header)
}

fn write_reply<H>(
    stream: &mut UnixStream,
    header: H,
) -> anyhow::Result<()>
    where H: serde::Serialize
{
    stream.set_write_timeout(Some(consts::SOCK_STREAM_TIMEOUT))
        .context("setting write timout on inbound session")?;

    let buf = rmp_serde::to_vec(&header).context("formatting reply header")?;
    stream.write_u32::<LittleEndian>(buf.len() as u32)
        .context("writing reply length prefix")?;
    stream.write_all(&buf).context("writing reply header")?;

    stream.set_write_timeout(None)
        .context("unsetting write timout on inbound session")?;
    Ok(())
}
