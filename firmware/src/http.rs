use core::fmt::Write as _;

use alloc::string::String;
use embassy_futures::select::{Either, select};
use embassy_net::tcp::TcpSocket;
use embassy_supervisor::{Aborted, ControlOp, Fault, InjectError, TaskNode};
use embassy_time::Duration;
use embedded_io_async::Write as _;

use crate::net;

const HTTP_PORT: u16 = 80;
const KEEPALIVE_IDLE_SECS: u64 = 10;

/// Minimum number of concurrent HTTP worker slots in the pool.
pub const HTTP_FLOOR: usize = 1;
/// Maximum number of concurrent HTTP worker slots in the pool.
pub const HTTP_CEIL: usize = 2;

/// A `hog` fault's default bound.
const HOG_DEFAULT_MS: u64 = 3000;
/// A `hog` fault's ceiling. The thread executor also runs the watchdog feeder,
/// so a hog must not outlast the 8 s bootloader watchdog feed.
const HOG_MAX_MS: u64 = 6000;

/// Counters tracked for each HTTP worker task.
pub struct WorkerStats {
    /// Number of requests served by this worker.
    pub served: u32,
}

embassy_supervisor::supervisor_fragment! {
    name: HTTP_FRAG;
    pool HTTP = [Terminate, OnDemand], deps: [NET ready],
        task: crate::http::http_task,
        resources: [HTTP_STATS: crate::http::WorkerStats,
                    NET_STACK: shared local embassy_net::Stack<'static>],
        state: zeroed crate::http::HttpBufs,
        dataflow: [crate::heartbeat::set_period_ms],
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(4)),
        min: crate::http::HTTP_FLOOR, max: crate::http::HTTP_CEIL,
        slot_timeout: 2000;
}

const INDEX_HTML: &str = "<!doctype html><meta charset=utf-8><title>task supervisor</title>\
<style>body{font-family:monospace;background:#111;color:#0f0;padding:1em}\
table{border-collapse:collapse}td,th{border:1px solid #333;padding:4px 8px;text-align:left}\
button,input,select{font-family:inherit;background:#1a1a1a;color:#0f0;border:1px solid #0a0}\
option{background:#1a1a1a;color:#0f0}button,select{cursor:pointer}</style>\
<h1>task supervisor</h1><div id=heap></div><div id=lf></div><table id=t></table>\
<h3>heartbeat</h3>\
<input id=hbms type=number value=500 style=width:5em> ms \
<button onclick=\"hb(hbms.value)\">blink</button> \
<button onclick=\"hb(0)\">off</button> \
<button onclick=\"hb(-1)\">on</button>\
<script>\
async function ctl(n,o){await fetch('/api/control?node='+n+'&op='+o,{method:'POST'});\
load();setTimeout(load,400);}\
async function hb(ms){await fetch('/api/heartbeat?ms='+ms,{method:'POST'});}\
async function flt(n,k){if(!k)return;await fetch('/api/fault?node='+n+'&kind='+k,{method:'POST'});\
load();setTimeout(load,400);}\
/* the same verbs as the playground's menu, plus a bounded hog */\
const fmenu=n=>'<select onchange=\"flt(\\''+n+'\\',this.value);this.value=\\'\\';this.blur()\">'+\
'<option value=\"\">⚡</option><option value=stall>stall (stop polling)</option>'+\
'<option value=wedge>wedge (no shutdown ack)</option><option value=crash>crash (abrupt exit)</option>'+\
'<option value=hog>hog 3 s</option><option value=clear>clear fault</option></select>';\
/* trace counters are raw wrapping u32 ticks: keep the previous sample and diff \
(>>>0 = wrap-safe) to turn them into rates; max_poll converts via tick_hz. */\
let prev=null;const sub=(a,b)=>(a-b)>>>0;\
async function load(){let d=await (await fetch('/api/tasks')).json();\
let f=d.heap_free,tot=d.heap_total,u=tot-f;\
let hd='heap: '+u+' used / '+f+' free / '+tot+' total B ('+Math.round(100*u/tot)+'% used)';\
if(prev&&prev.x)for(let e of d.executors||[]){let p=prev.x[e.id];\
if(p!=null){let dt=sub(d.now_ticks,prev.now);\
let busy=Math.max(0,100*(1-sub(e.idle_ticks,p.i)/dt));\
let poll=100*sub(e.exec_ticks,p.e)/dt;\
hd+=' | executor '+(e.id>>>0).toString(16)+': '+busy.toFixed(1)+'% busy ('+\
poll.toFixed(1)+'% in-poll, '+Math.max(0,busy-poll).toFixed(1)+'% overhead, '+\
Math.round(sub(e.polls,p.p)/(dt/d.tick_hz))+' polls/s)';}}\
document.getElementById('heap').textContent=hd;\
document.getElementById('lf').textContent=d.last_fault?'last supervisor fault: '+d.last_fault:'';\
const cpu=(n,tk)=>(!prev||prev.m[n]==null)?'-':\
(100*sub(tk,prev.m[n])/sub(d.now_ticks,prev.now)).toFixed(1)+'%';\
const us=t=>Math.round(t*1e6/d.tick_hz)+'us';\
let h='<tr><th>task<th>mode<th>state<th>gen<th>cpu<th>max poll<th>deps<th>';\
let pool=null;\
for(let t of d.tasks){\
if(/^http[0-9]+$/.test(t.name)){\
pool=pool||{n:0,r:0,b:0,dis:true,deps:t.deps,e:0,mp:0};\
pool.n++;if(t.running)pool.r++;if(t.busy)pool.b++;if(!t.disabled)pool.dis=false;\
pool.e=(pool.e+t.exec_ticks)>>>0;pool.mp=Math.max(pool.mp,t.max_poll_ticks);continue;}\
let st=t.detached?'detached':t.disabled?'disabled':t.collateral?'held':\
t.bound_stopped?'link-stopped':\
t.running?(t.ready===false?'up (not ready)':t.busy?'busy':'running'):'stopped';\
if(t.fault)st+=' ⚡'+t.fault;\
let pause=t.mode=='pause';let act=t.disabled||!t.running;\
let op=act?(pause?'resume':'start'):(pause?'pause':'stop');\
h+='<tr><td>'+t.name+'<td>'+t.mode+'<td>'+st+'<td>'+t.epoch+'<td>'+cpu(t.name,t.exec_ticks)+\
'<td>'+us(t.max_poll_ticks)+'<td>'+t.deps.join(',')+\
'<td>'+(t.detached?'':'<button onclick=\"ctl(\\''+t.name+'\\',\\''+op+'\\')\">'+op+'</button>'+\
'<button onclick=\"ctl(\\''+t.name+'\\',\\'restart\\')\">restart</button>'+fmenu(t.name));}\
if(pool){let op=pool.dis?'start':'stop';\
h+='<tr><td>http (pool)<td>elastic<td>'+pool.r+'/'+pool.n+' up, '+pool.b+' busy<td>-<td>'+\
cpu('_pool',pool.e)+'<td>'+us(pool.mp)+'<td>'+pool.deps.join(',')+\
'<td><button onclick=\"ctl(\\'http0\\',\\''+op+'\\')\">'+op+'</button>'+fmenu('http0');}\
/* an open menu lives in this table: rewriting it would close the menu, so \
hold the redraw while one has focus (the choice blurs it and redraws) */\
const ae=document.activeElement;if(ae&&ae.tagName=='SELECT'&&ae.closest('#t'))return;\
document.getElementById('t').innerHTML=h;\
let m={};for(let t of d.tasks)m[t.name]=t.exec_ticks;if(pool)m['_pool']=pool.e;\
let x={};for(let e of d.executors||[])x[e.id]={i:e.idle_ticks,e:e.exec_ticks,p:e.polls};\
prev={now:d.now_ticks,m:m,x:x};}\
load();setInterval(load,2000);\
</script>";

/// Preallocated buffers used by an HTTP worker.
pub struct HttpBufs {
    rx: [u8; 1024],
    tx: [u8; 2560],
    req: [u8; 1024],
}
unsafe impl embassy_supervisor::Zeroable for HttpBufs {}

pub(crate) async fn http_task(
    node: &'static TaskNode,
    stats: &mut WorkerStats,
    stack: embassy_net::Stack<'static>,
    bufs: &mut HttpBufs,
) {
    let Some(_held) = net::hold() else {
        return;
    };
    let HttpBufs { rx, tx, req } = bufs;

    loop {
        if node.shutdown_requested() {
            node.ack_dropped();
            return;
        }

        let mut socket = TcpSocket::new(stack, &mut rx[..], &mut tx[..]);
        socket.set_timeout(Some(Duration::from_secs(KEEPALIVE_IDLE_SECS)));

        match node.run_cancellable(socket.accept(HTTP_PORT)).await {
            Err(Aborted) => {
                node.ack_dropped();
                return;
            }
            Ok(Err(_)) => continue,
            Ok(Ok(())) => {}
        }

        node.mark_busy();
        serve_connection(&mut socket, &mut req[..], node).await;
        stats.served = stats.served.wrapping_add(1);
        socket.close();
        let _ = socket.flush().await;
        node.mark_idle();
    }
}

async fn serve_connection(socket: &mut TcpSocket<'_>, req: &mut [u8], node: &'static TaskNode) {
    loop {
        if node.shutdown_requested() {
            return;
        }
        let n = match select(socket.read(req), node.wait_shutdown()).await {
            Either::Second(()) => return,
            Either::First(Ok(0)) | Either::First(Err(_)) => return,
            Either::First(Ok(n)) => n,
        };

        let request = core::str::from_utf8(&req[..n]).unwrap_or("");
        let line = request.lines().next().unwrap_or("");
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");
        let keep = !connection_close_requested(request);

        match (method, path) {
            ("GET", "/") => send(socket, "text/html", INDEX_HTML, keep).await,
            ("GET", "/api/tasks") => {
                let mut body = String::with_capacity(4096);
                build_tasks_json(&mut body);
                send(socket, "application/json", &body, keep).await;
            }
            ("GET", "/api/bench") => {
                let mut body = String::with_capacity(48);
                match crate::BENCH_EXIT.take() {
                    Some(slices) => {
                        let _ = write!(body, "{{\"slices\":{}}}", slices);
                    }
                    None => body.push_str("{\"slices\":null}"),
                }
                send(socket, "application/json", &body, keep).await;
            }
            ("POST", p) if p.starts_with("/api/control") => match handle_control(p) {
                Ok(body) => send(socket, "application/json", &body, keep).await,
                Err(body) => {
                    send_status(
                        socket,
                        "503 Service Unavailable",
                        "application/json",
                        &body,
                        keep,
                    )
                    .await
                }
            },
            ("POST", p) if p.starts_with("/api/heartbeat") => {
                let body = handle_heartbeat(p, node);
                send(socket, "application/json", &body, keep).await;
            }
            ("POST", p) if p.starts_with("/api/fault") => {
                let body = handle_fault(p);
                send(socket, "application/json", &body, keep).await;
            }
            ("POST", p) if p.starts_with("/api/ota") => {
                match crate::ota::set_target(query(p, "ip="), query(p, "port="), query(p, "path="))
                {
                    Ok(()) => {
                        send(
                            socket,
                            "application/json",
                            "{\"accepted\":true,\"status\":\"downloading\"}",
                            false,
                        )
                        .await;
                        embassy_supervisor::request_control(&crate::OTA, ControlOp::Activate).await;
                        return;
                    }
                    Err(e) => {
                        let mut out = String::with_capacity(64);
                        let _ = write!(out, "{{\"accepted\":false,\"error\":\"{}\"}}", e);
                        send(socket, "application/json", &out, keep).await;
                    }
                }
            }
            _ => {
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                return;
            }
        }

        if !keep {
            return;
        }
    }
}

async fn send(socket: &mut TcpSocket<'_>, content_type: &str, body: &str, keep: bool) {
    send_status(socket, "200 OK", content_type, body, keep).await;
}

/// Like [`send`] with an explicit status line — the control endpoint answers
/// `503 Service Unavailable` when the supervisor's mailbox is full.
async fn send_status(
    socket: &mut TcpSocket<'_>,
    status: &str,
    content_type: &str,
    body: &str,
    keep: bool,
) {
    // Heap-built so the header isn't held inline in the worker future across the
    let mut header = String::with_capacity(128);
    let _ = write!(
        header,
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: {}\r\n\r\n",
        status,
        content_type,
        body.len(),
        if keep { "keep-alive" } else { "close" },
    );
    let _ = socket.write_all(header.as_bytes()).await;
    let _ = socket.write_all(body.as_bytes()).await;
}

fn connection_close_requested(request: &str) -> bool {
    for l in request.lines() {
        let l = l.trim_start();
        const KEY: &str = "connection:";
        if l.len() >= KEY.len() && l[..KEY.len()].eq_ignore_ascii_case(KEY) {
            return contains_ascii_ci(&l[KEY.len()..], "close");
        }
    }
    false
}

fn contains_ascii_ci(haystack: &str, needle_lower: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle_lower.as_bytes());
    if n.is_empty() {
        return true;
    }
    if h.len() < n.len() {
        return false;
    }
    (0..=h.len() - n.len()).any(|i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

fn build_tasks_json(json: &mut String) {
    let _ = write!(
        json,
        "{{\"heap_free\":{},\"heap_total\":{},\"tick_hz\":{},\"now_ticks\":{},",
        crate::heap::free_bytes(),
        crate::heap::HEAP_SIZE,
        embassy_time::TICK_HZ,
        embassy_time::Instant::now().as_ticks() as u32,
    );
    match crate::LAST_FAULT.lock(|c| c.get()) {
        Some((node, kind)) => {
            let _ = write!(json, "\"last_fault\":\"{node}: {kind}\",");
        }
        None => json.push_str("\"last_fault\":null,"),
    }
    json.push_str("\"executors\":[");
    let mut first = true;
    for id in embassy_supervisor::trace::executors() {
        if id == 0 {
            continue;
        }
        let Some(st) = embassy_supervisor::trace::executor_stats(id) else {
            continue;
        };
        if !first {
            json.push(',');
        }
        first = false;
        let _ = write!(
            json,
            "{{\"id\":{},\"idle_ticks\":{},\"exec_ticks\":{},\"polls\":{},\"passes\":{}}}",
            id, st.idle_ticks, st.exec_ticks, st.polls, st.passes
        );
    }
    json.push_str("],\"tasks\":[");
    let mut first = true;
    for (i, slot) in crate::GRAPH.nodes.iter().enumerate() {
        let Some(node) = slot else {
            continue;
        };
        if !first {
            json.push(',');
        }
        first = false;
        write_task(json, node, crate::GRAPH.deps_of(i as u8));
    }
    if let Some(node) = crate::GRAPH.graph_ref.self_node() {
        if !first {
            json.push(',');
        }
        write_task(json, node, &[]);
    }
    json.push_str("]}");
}

fn write_task(json: &mut String, node: &'static TaskNode, deps: &'static [u8]) {
    let _ = write!(
        json,
        "{{\"name\":\"{}\",\"mode\":\"{}\",\"running\":{},\"busy\":{},\"disabled\":{},\
         \"collateral\":{},\"detached\":{},\
         \"ready\":{},\"bound_stopped\":{},\"epoch\":{},\
         \"exec_ticks\":{},\"polls\":{},\"max_poll_ticks\":{},",
        node.name(),
        node.mode().as_str(),
        node.is_running(),
        node.is_busy(),
        node.is_disabled(),
        node.is_collateral(),
        node.is_detached(),
        node.is_ready(),
        node.is_bound_stopped(),
        node.epoch(),
        node.exec_ticks(),
        node.poll_count(),
        node.max_poll_ticks()
    );
    match node.fault() {
        Fault::None => json.push_str("\"fault\":null,"),
        f => {
            let _ = write!(json, "\"fault\":\"{}\",", f.as_str());
        }
    }
    match node.status() {
        Some(s) => {
            let _ = write!(json, "\"status\":\"{s}\",\"deps\":[");
        }
        None => json.push_str("\"status\":null,\"deps\":["),
    }
    for (j, &di) in deps.iter().enumerate() {
        if j > 0 {
            json.push(',');
        }
        if let Some(dep) = crate::GRAPH.nodes[di as usize] {
            let _ = write!(json, "\"{}\"", dep.name());
        }
    }
    json.push_str("]}");
}

fn handle_control(path: &str) -> Result<String, String> {
    let node_name = query(path, "node=");
    let op_str = query(path, "op=");
    let op = match op_str {
        "start" | "resume" => Some(ControlOp::Activate),
        "stop" | "pause" => Some(ControlOp::Deactivate),
        "restart" => Some(ControlOp::Restart),
        _ => None,
    };
    let node = crate::GRAPH
        .nodes
        .iter()
        .copied()
        .flatten()
        .find(|n| n.name() == node_name);

    let mut out = String::with_capacity(96);
    match (node, op) {
        (Some(node), Some(op)) => {
            if embassy_supervisor::try_request_control(node, op).is_err() {
                out.push_str("{\"accepted\":false,\"error\":\"control queue full\"}");
                return Err(out);
            }
            let _ = write!(
                out,
                "{{\"accepted\":true,\"node\":\"{}\",\"op\":\"{}\"}}",
                node.name(),
                op_str
            );
        }
        _ => {
            out.push_str("{\"accepted\":false,\"error\":\"unknown node or op\"}");
        }
    }
    Ok(out)
}

/// `POST /api/fault?node=NAME&kind=KIND[&ms=N]`. Inject a fault or clear it.
/// `hog` takes an optional bound in ms, capped at `HOG_MAX_MS`.
fn handle_fault(path: &str) -> String {
    let node_name = query(path, "node=");
    let kind = query(path, "kind=");
    let node = crate::GRAPH
        .nodes
        .iter()
        .copied()
        .flatten()
        .find(|n| n.name() == node_name);
    let fault = match kind {
        "stall" => Some(Fault::Stall),
        "wedge" => Some(Fault::Wedge),
        "crash" => Some(Fault::Crash),
        "hog" => {
            let ms = query(path, "ms=")
                .parse::<u64>()
                .unwrap_or(HOG_DEFAULT_MS)
                .min(HOG_MAX_MS);
            Some(Fault::Hog(Duration::from_millis(ms)))
        }
        "clear" => Some(Fault::None),
        _ => None,
    };
    let mut out = String::with_capacity(96);
    match (node, fault) {
        (Some(node), Some(fault)) => match node.inject(fault) {
            Ok(()) => {
                let _ = write!(
                    out,
                    "{{\"accepted\":true,\"node\":\"{}\",\"kind\":\"{}\"}}",
                    node.name(),
                    kind
                );
            }
            Err(InjectError::NoShell) => {
                out.push_str("{\"accepted\":false,\"error\":\"no shell\"}");
            }
            #[allow(unreachable_patterns)]
            Err(_) => out.push_str("{\"accepted\":false,\"error\":\"refused\"}"),
        },
        _ => out.push_str("{\"accepted\":false,\"error\":\"unknown node or kind\"}"),
    }
    out
}

fn handle_heartbeat(path: &str, node: &'static TaskNode) -> String {
    let mut out = String::with_capacity(64);
    match query(path, "ms=").parse::<i32>() {
        Ok(ms) => {
            crate::heartbeat::set_period_ms(node, ms);
            let mode = if ms > 0 {
                "blink"
            } else if ms == 0 {
                "off"
            } else {
                "on"
            };
            let _ = write!(
                out,
                "{{\"accepted\":true,\"ms\":{},\"mode\":\"{}\"}}",
                ms, mode
            );
        }
        Err(_) => out.push_str("{\"accepted\":false,\"error\":\"bad or missing ms\"}"),
    }
    out
}

/// Extract the value of `key` (e.g. `"node="`) from a query string, up to the
/// next `&`/space/EOL.
fn query<'a>(path: &'a str, key: &str) -> &'a str {
    path.split(key)
        .nth(1)
        .and_then(|s| s.split(['&', ' ', '\r', '\n']).next())
        .unwrap_or("")
}
