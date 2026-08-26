use crate::*;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone)]
enum OutputEvent { Data(Vec<u8>), Exit(ExitInfo), Error(String) }

struct HubInner {
    history: History,
    subscribers: HashMap<u64, mpsc::Sender<OutputEvent>>,
    next_id: u64,
    final_exit: Option<ExitInfo>,
}

struct OutputHub { inner: Mutex<HubInner> }
impl OutputHub {
    fn new(history: History) -> Self {
        Self { inner: Mutex::new(HubInner { history, subscribers: HashMap::new(), next_id: 1, final_exit: None }) }
    }
    fn append(&self, data: &[u8]) -> Result<()> {
        let mut inner = lock(&self.inner)?;
        inner.history.append(data)?;
        inner.subscribers.retain(|_, tx| tx.send(OutputEvent::Data(data.to_vec())).is_ok());
        Ok(())
    }
    fn snapshot(&self, max: Option<usize>) -> Result<Vec<u8>> { Ok(lock(&self.inner)?.history.snapshot(max)) }
    fn subscribe(&self, max: Option<usize>) -> Result<(u64, Vec<u8>, mpsc::Receiver<OutputEvent>)> {
        let mut inner = lock(&self.inner)?;
        let history = inner.history.snapshot(max);
        let id = inner.next_id; inner.next_id += 1;
        let (tx, rx) = mpsc::channel();
        if let Some(exit) = inner.final_exit.clone() { let _ = tx.send(OutputEvent::Exit(exit)); }
        else { inner.subscribers.insert(id, tx); }
        Ok((id, history, rx))
    }
    fn unsubscribe(&self, id: u64) { if let Ok(mut inner) = self.inner.lock() { inner.subscribers.remove(&id); } }
    fn finish(&self, exit: ExitInfo) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.final_exit = Some(exit.clone());
            for (_, tx) in inner.subscribers.drain() { let _ = tx.send(OutputEvent::Exit(exit.clone())); }
        }
    }
    fn fail_subscribers(&self, message: String) {
        if let Ok(mut inner) = self.inner.lock() {
            for (_, tx) in inner.subscribers.drain() { let _ = tx.send(OutputEvent::Error(message.clone())); }
        }
    }
}

#[derive(Debug)]
struct WorkloadState { running: bool, pid: u32, pgid: i32 }

struct WorkerRuntime {
    record_path: std::path::PathBuf,
    record: Mutex<SessionRecord>,
    pty_write: Mutex<Option<File>>,
    workload: Mutex<WorkloadState>,
    cgroup: Mutex<Option<Cgroup>>,
    kill_gate: Mutex<()>,
    output: OutputHub,
}

impl WorkerRuntime {
    fn record(&self) -> Result<SessionRecord> { Ok(lock(&self.record)?.clone()) }
    fn update_record<F>(&self, update: F) -> Result<SessionRecord> where F: FnOnce(&mut SessionRecord) {
        let mut record = lock(&self.record)?;
        update(&mut record);
        record.updated_at_ms = now_ms();
        atomic_write_json(&self.record_path, &*record)?;
        Ok(record.clone())
    }
    fn send(&self, data: &[u8]) -> Result<()> {
        if !lock(&self.workload)?.running { bail!("workload has exited"); }
        let mut pty = lock(&self.pty_write)?;
        let file = pty.as_mut().ok_or_else(|| anyhow!("PTY is closed"))?;
        file.write_all(data).context("write PTY")?;
        file.flush()?;
        Ok(())
    }
    fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let pty = lock(&self.pty_write)?;
        let file = pty.as_ref().ok_or_else(|| anyhow!("PTY is closed"))?;
        set_winsize(file.as_raw_fd(), rows.max(1), cols.max(1))
    }
    fn signal(&self, signal: i32) -> Result<()> {
        let workload = lock(&self.workload)?;
        if !workload.running { bail!("workload has exited"); }
        if unsafe { libc::kill(-workload.pgid, signal) } != 0 {
            return Err(io::Error::last_os_error()).context("signal process group");
        }
        Ok(())
    }
    fn kill(&self, signal: i32, grace_ms: u64) -> Result<()> {
        let _serialized = lock(&self.kill_gate)?;
        let (running, pgid) = { let state=lock(&self.workload)?; (state.running, state.pgid) };
        if !running { return Ok(()); }
        let cgroup = lock(&self.cgroup)?.clone();
        if signal == libc::SIGKILL {
            if let Some(cg) = &cgroup { cg.kill_all()?; }
            else if unsafe { libc::kill(-pgid, libc::SIGKILL) } != 0 { return Err(io::Error::last_os_error()).context("kill process group"); }
            return Ok(());
        }
        if let Some(cg) = &cgroup { cg.signal_all(signal)?; }
        else if unsafe { libc::kill(-pgid, signal) } != 0 { return Err(io::Error::last_os_error()).context("signal process group"); }
        if grace_ms > 0 { thread::sleep(Duration::from_millis(grace_ms)); }
        let still_running = lock(&self.workload)?.running;
        if still_running {
            if let Some(cg) = &cgroup { cg.kill_all()?; }
            else { unsafe { libc::kill(-pgid, libc::SIGKILL); } }
        }
        Ok(())
    }
    fn rename(&self, workspace: std::path::PathBuf, tag: String) -> Result<SessionRecord> {
        validate_tag(&tag)?;
        let workspace = canonical_workspace(&workspace)?;
        self.update_record(|r| { r.workspace=workspace; r.tag=tag; })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> { mutex.lock().map_err(|_| anyhow!("worker lock poisoned")) }

enum LifeEvent {
    PtyEof,
    PtyError(String),
    ChildExit { code: Option<i32>, signal: Option<i32> },
}

pub fn run_worker(id: Uuid) -> Result<()> {
    let paths = Paths::discover()?;
    let record_path = paths.record(id);
    let mut record = read_record(&record_path)?;
    ensure_private_dir(&paths.runtime_session(id))?;
    let _worker_lock = FileLock::exclusive(&paths.worker_lock(id), true)
        .with_context(|| format!("worker for {id} is already running"))?;

    record.worker_pid = Some(std::process::id());
    record.updated_at_ms = now_ms();
    atomic_write_json(&record_path, &record)?;

    let socket_path = paths.socket(id);
    if socket_path.exists() { fs::remove_file(&socket_path).context("remove stale control socket")?; }
    let listener = UnixListener::bind(&socket_path).with_context(|| format!("bind {}", socket_path.display()))?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;

    let cgroup = match Cgroup::create(id, &record.limits) {
        Ok(value) => value,
        Err(error) => {
            record.phase = Phase::Failed; record.error = Some(format!("{error:#}")); record.updated_at_ms=now_ms();
            atomic_write_json(&record_path, &record)?;
            return Err(error);
        }
    };
    let (master_read, slave) = open_pty(24, 80)?;
    let master_write = master_read.try_clone()?;
    let child = match spawn_workload(&record, master_read.as_raw_fd(), slave, cgroup.as_ref()) {
        Ok(child) => child,
        Err(error) => {
            record.phase=Phase::Failed; record.error=Some(format!("{error:#}")); record.updated_at_ms=now_ms();
            atomic_write_json(&record_path, &record)?;
            return Err(error);
        }
    };
    let pid = child.id();
    record.workload_pid=Some(pid); record.phase=Phase::Running; record.updated_at_ms=now_ms(); record.error=None;
    atomic_write_json(&record_path, &record)?;

    let history = History::open(record.history_path.clone(), record.history_bytes)?;
    let runtime = Arc::new(WorkerRuntime {
        record_path, record: Mutex::new(record), pty_write: Mutex::new(Some(master_write)),
        workload: Mutex::new(WorkloadState { running: true, pid, pgid: pid as i32 }),
        cgroup: Mutex::new(cgroup), kill_gate: Mutex::new(()), output: OutputHub::new(history),
    });
    let (life_tx, life_rx) = mpsc::channel();
    spawn_pty_reader(master_read, runtime.clone(), life_tx.clone());
    spawn_child_waiter(child, life_tx);
    spawn_lifecycle(runtime.clone(), life_rx);

    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let runtime = runtime.clone();
                thread::spawn(move || { if let Err(error) = handle_connection(stream, runtime) { eprintln!("aplexer connection: {error:#}"); } });
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("accept control connection"),
        }
    }
}

fn spawn_workload(record: &SessionRecord, master_fd: RawFd, slave: File, cgroup: Option<&Cgroup>) -> Result<Child> {
    let program = record.command.first().ok_or_else(|| anyhow!("empty workload command"))?;
    let mut gate = [0i32; 2];
    if unsafe { libc::pipe2(gate.as_mut_ptr(), libc::O_CLOEXEC) } != 0 { return Err(io::Error::last_os_error()).context("pipe2"); }
    let gate_read=gate[0]; let gate_write=gate[1]; let slave_fd=slave.as_raw_fd();
    let mut command=Command::new(program);
    command.args(&record.command[1..]).current_dir(&record.cwd).envs(&record.env)
        .env("APLEXER_SESSION_ID", record.id.to_string())
        .env("APLEXER_WORKSPACE", &record.workspace)
        .env("APLEXER_TAG", &record.tag)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    unsafe {
        command.pre_exec(move || {
            libc::close(master_fd); libc::close(gate_write);
            if libc::setsid() < 0 { return Err(io::Error::last_os_error()); }
            if libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) < 0 { return Err(io::Error::last_os_error()); }
            for target in 0..=2 { if libc::dup2(slave_fd, target) < 0 { return Err(io::Error::last_os_error()); } }
            if slave_fd > 2 { libc::close(slave_fd); }
            let pgid=libc::getpid(); libc::tcsetpgrp(0, pgid);
            let mut byte=0u8;
            loop {
                let n=libc::read(gate_read, &mut byte as *mut _ as *mut _, 1);
                if n == 1 { break; }
                if n == 0 { return Err(io::Error::new(io::ErrorKind::BrokenPipe, "launch gate closed")); }
                let error=io::Error::last_os_error(); if error.kind()!=io::ErrorKind::Interrupted { return Err(error); }
            }
            libc::close(gate_read);
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => { unsafe { libc::close(gate_read); libc::close(gate_write); } return Err(error).context("spawn workload"); }
    };
    drop(slave);
    unsafe { libc::close(gate_read); }
    if let Some(cgroup)=cgroup {
        if let Err(error)=cgroup.add_pid(child.id()) {
            unsafe { libc::kill(child.id() as i32, libc::SIGKILL); libc::close(gate_write); }
            let _=child.wait(); return Err(error);
        }
    }
    let byte=[1u8];
    let written=unsafe { libc::write(gate_write, byte.as_ptr() as *const _, 1) };
    unsafe { libc::close(gate_write); }
    if written != 1 { unsafe { libc::kill(child.id() as i32, libc::SIGKILL); } let _=child.wait(); bail!("release workload launch gate failed"); }
    Ok(child)
}

fn spawn_pty_reader(mut master: File, runtime: Arc<WorkerRuntime>, tx: mpsc::Sender<LifeEvent>) {
    thread::spawn(move || {
        let mut buffer=vec![0u8; 32*1024];
        loop {
            match master.read(&mut buffer) {
                Ok(0) => { let _=tx.send(LifeEvent::PtyEof); break; }
                Ok(n) => if let Err(error)=runtime.output.append(&buffer[..n]) { let _=tx.send(LifeEvent::PtyError(format!("persist output: {error:#}"))); break; },
                Err(error) if error.kind()==io::ErrorKind::Interrupted => continue,
                Err(error) if error.raw_os_error()==Some(libc::EIO) => { let _=tx.send(LifeEvent::PtyEof); break; }
                Err(error) => { let _=tx.send(LifeEvent::PtyError(format!("read PTY: {error}"))); break; }
            }
        }
    });
}

fn spawn_child_waiter(mut child: Child, tx: mpsc::Sender<LifeEvent>) {
    thread::spawn(move || {
        let event=match child.wait() {
            Ok(status) => LifeEvent::ChildExit { code: status.code(), signal: status.signal() },
            Err(error) => LifeEvent::PtyError(format!("wait workload: {error}")),
        };
        let _=tx.send(event);
    });
}

fn spawn_lifecycle(runtime: Arc<WorkerRuntime>, rx: mpsc::Receiver<LifeEvent>) {
    thread::spawn(move || {
        let mut pty_eof=false; let mut child_exit: Option<(Option<i32>,Option<i32>)>=None; let mut fatal: Option<String>=None;
        while let Ok(event)=rx.recv() {
            match event {
                LifeEvent::PtyEof => { pty_eof=true; if let Ok(mut pty)=runtime.pty_write.lock() { *pty=None; } },
                LifeEvent::PtyError(message) => { pty_eof=true; fatal=Some(message.clone()); if let Ok(mut pty)=runtime.pty_write.lock(){*pty=None;} runtime.output.fail_subscribers(message); },
                LifeEvent::ChildExit { code, signal } => {
                    child_exit=Some((code,signal));
                    if let Ok(mut state)=runtime.workload.lock(){state.running=false;}
                    let _=runtime.update_record(|r| r.phase=Phase::Exiting);
                }
            }
            if pty_eof && child_exit.is_some() { break; }
        }
        let (code,signal)=child_exit.unwrap_or((None,None));
        let (oom,cg)=match runtime.cgroup.lock() { Ok(mut g)=>{let cg=g.take(); let oom=cg.as_ref().map(Cgroup::oom_killed).unwrap_or(false);(oom,cg)},Err(_)=>(false,None) };
        let exit=ExitInfo { code, signal, oom_killed:oom, exited_at_ms:now_ms() };
        let error=fatal.clone();
        let _=runtime.update_record(|r| { r.phase=if error.is_some(){Phase::Failed}else{Phase::Exited}; r.exit=Some(exit.clone()); r.error=error; });
        runtime.output.finish(exit);
        if let Some(cg)=cg { cg.cleanup(); }
    });
}

fn handle_connection(mut stream: UnixStream, runtime: Arc<WorkerRuntime>) -> Result<()> {
    let uid=peer_uid(stream.as_raw_fd())?;
    if uid != unsafe { libc::geteuid() } { bail!("peer uid {uid} is not authorized"); }
    let frame=read_frame(&mut stream)?.ok_or_else(|| anyhow!("empty request"))?;
    let request: Request=frame_json(frame)?;
    if request.version!=PROTOCOL_VERSION { write_json(&mut stream,&Response::error(request.request_id,"unsupported protocol version"))?; return Ok(()); }
    let id=request.request_id.clone();
    match request.operation {
        Operation::Ping => write_json(&mut stream,&Response::ok(id,json!({"pong":true})))?,
        Operation::Status => write_json(&mut stream,&Response::ok(id,serde_json::to_value(runtime.record()?)?))?,
        Operation::Send { bytes } => {
            let next=read_frame(&mut stream)?.ok_or_else(|| anyhow!("missing data frame"))?;
            if next.kind!=FrameKind::Data || next.payload.len()!=bytes { write_json(&mut stream,&Response::error(id,"data length mismatch"))?; }
            else { match runtime.send(&next.payload) { Ok(())=>write_json(&mut stream,&Response::ok(id,json!({"bytes":bytes})))?, Err(e)=>write_json(&mut stream,&Response::error(id,format!("{e:#}")))? } }
        }
        Operation::Capture { max_bytes } => {
            let data=runtime.output.snapshot(max_bytes)?;
            write_json(&mut stream,&Response::ok(id,json!({"bytes":data.len()})))?;
            write_frame(&mut stream,FrameKind::Data,&data)?;
        }
        Operation::Attach { history_bytes } => handle_attach(stream,runtime,id,history_bytes)?,
        Operation::Resize { rows, cols } => match runtime.resize(rows,cols) { Ok(())=>write_json(&mut stream,&Response::ok(id,json!({})))?,Err(e)=>write_json(&mut stream,&Response::error(id,format!("{e:#}")))? },
        Operation::Kill { signal, grace_ms } => match runtime.kill(signal,grace_ms) { Ok(())=>write_json(&mut stream,&Response::ok(id,json!({"signalled":true})))?,Err(e)=>write_json(&mut stream,&Response::error(id,format!("{e:#}")))? },
        Operation::Rename { workspace, tag } => match runtime.rename(workspace,tag) { Ok(record)=>write_json(&mut stream,&Response::ok(id,serde_json::to_value(record)?))?,Err(e)=>write_json(&mut stream,&Response::error(id,format!("{e:#}")))? },
    }
    Ok(())
}

fn handle_attach(mut reader: UnixStream, runtime: Arc<WorkerRuntime>, request_id: String, max: Option<usize>) -> Result<()> {
    let (subscription, history, rx)=runtime.output.subscribe(max)?;
    let writer_stream=reader.try_clone()?;
    let writer=Arc::new(Mutex::new(writer_stream));
    {
        let mut out=lock(&writer)?;
        write_json(&mut *out,&Response::ok(request_id,json!({"attached":true,"history_bytes":history.len()})))?;
        write_frame(&mut *out,FrameKind::Data,&history)?;
    }
    let output_writer=writer.clone(); let output_runtime=runtime.clone();
    thread::spawn(move || {
        while let Ok(event)=rx.recv() {
            let result=(|| -> Result<bool> {
                let mut out=lock(&output_writer)?;
                match event {
                    OutputEvent::Data(data)=>{write_frame(&mut *out,FrameKind::Data,&data)?;Ok(true)},
                    OutputEvent::Exit(exit)=>{write_json(&mut *out,&ServerEvent::Exit{exit})?;Ok(false)},
                    OutputEvent::Error(message)=>{write_json(&mut *out,&ServerEvent::Error{message})?;Ok(false)},
                }
            })();
            if !matches!(result,Ok(true)){break;}
        }
        output_runtime.output.unsubscribe(subscription);
        if let Ok(out)=output_writer.lock(){let _=out.shutdown(std::net::Shutdown::Both);}
    });
    loop {
        let frame=match read_frame(&mut reader) { Ok(Some(f))=>f, Ok(None)=>break, Err(_)=>break };
        match frame.kind {
            FrameKind::Data => { if runtime.send(&frame.payload).is_err(){break;} },
            FrameKind::End => break,
            FrameKind::Json => {
                let control: AttachControl=serde_json::from_slice(&frame.payload)?;
                match control {
                    AttachControl::Resize{rows,cols}=>{let _=runtime.resize(rows,cols);},
                    AttachControl::Signal{signal}=>{let _=runtime.signal(signal);},
                    AttachControl::Detach=>break,
                }
            }
        }
    }
    runtime.output.unsubscribe(subscription);
    let _=reader.shutdown(std::net::Shutdown::Both);
    Ok(())
}
