mod agent;
mod keyfile;
mod model;
mod operations;
mod profiles;
mod snapshot;
mod traffic;
mod vault;
use anyhow::Result;
use futures_util::StreamExt;
pub use model::*;
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};
use zbus::{zvariant::OwnedValue, Connection, Proxy};
const NM: &str = "org.freedesktop.NetworkManager";
const ROOT: &str = "/org/freedesktop/NetworkManager";
type Settings = HashMap<String, HashMap<String, OwnedValue>>;
async fn proxy<'a>(
    connection: &'a Connection,
    path: &'a str,
    interface: &'a str,
) -> Result<Proxy<'a>> {
    Ok(zbus::proxy::Builder::new(connection)
        .destination(NM)?
        .path(path)?
        .interface(interface)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?)
}
async fn properties(
    connection: &Connection,
    path: &str,
    interface: &str,
) -> Result<HashMap<String, OwnedValue>> {
    Ok(proxy(connection, path, "org.freedesktop.DBus.Properties")
        .await?
        .call("GetAll", &(interface,))
        .await?)
}
pub async fn snapshot(connection: &Connection) -> Result<Snapshot> {
    snapshot::load(connection).await
}

struct Pending {
    key: String,
    active: String,
    profile: String,
    device: String,
    started: Instant,
    operation: u64,
    editor: bool,
    wireless: bool,
}
struct Job {
    key: String,
    profile: String,
    device: String,
    editor: bool,
    wireless: bool,
    handle: tokio::task::JoinHandle<()>,
}
fn action_context(action: &Action, state: &Snapshot) -> (String, String, bool) {
    match action {
        Action::Connect { path, .. } => state
            .networks
            .iter()
            .find(|n| n.path == *path || n.profile_path == *path)
            .map(|n| {
                (
                    n.profile_path.clone(),
                    n.device.clone(),
                    n.device_kind == 2 || !n.wired,
                )
            })
            .unwrap_or_default(),
        Action::ActivateProfile { path, device } => (
            path.clone(),
            device.clone(),
            state
                .profiles
                .iter()
                .find(|p| p.path == *path || p.uuid == *path)
                .is_some_and(|p| p.kind == "802-11-wireless"),
        ),
        Action::SaveProfile(d)
        | Action::ConnectProfile { draft: d }
        | Action::Hidden(d)
        | Action::Hotspot(d) => {
            let device = state
                .devices
                .iter()
                .find(|a| a.path == d.device || a.iface == d.device)
                .map(|a| a.path.clone())
                .unwrap_or_default();
            (
                d.path.clone(),
                device,
                matches!(d.kind.as_str(), "802-11-wireless" | "hotspot"),
            )
        }
        Action::Refresh => (String::new(), String::new(), true),
        _ => Default::default(),
    }
}
struct Completed {
    generation: u64,
    id: u64,
    target: String,
    result: Result<operations::Applied>,
}
fn action_error(error: &anyhow::Error) -> String {
    let text = error.to_string();
    if text.contains("NotAuthorized")
        || text.contains("PermissionDenied")
        || text.contains("AccessDenied")
    {
        "Permission to change network settings was denied".into()
    } else if error.downcast_ref::<zbus::Error>().is_some() {
        "NetworkManager rejected the request. Check the adapter and network profile settings".into()
    } else {
        text
    }
}
fn unavailable(error: String) -> Snapshot {
    Snapshot {
        error,
        operation_done: true,
        ..Default::default()
    }
}
fn mark_operation(state: &mut Snapshot, id: u64, kind: &str, target: &str) {
    state.operation_id = id;
    state.operation_kind = kind.into();
    state.operation_target = target.into();
    state.operation_done = false;
    state.operation_error.clear();
    state.error.clear();
}
fn finish_operation(state: &mut Snapshot, id: u64, error: &str) {
    if state.operation_id == id {
        state.operation_done = true;
        state.operation_error = error.into();
    }
    state.error = error.into();
}
fn overlay_pending(state: &mut Snapshot, pending: &HashMap<u64, Pending>) {
    state.pending = pending
        .values()
        .filter(|p| {
            !state.networks.iter().any(|n| {
                (n.path == p.key || (!p.profile.is_empty() && (n.profile_path == p.profile || n.saved == p.profile))) && n.active
            })
        })
        .min_by_key(|p| p.operation)
        .map(|p| p.key.clone())
        .unwrap_or_default();
    for row in &mut state.networks {
        if pending
            .values()
            .any(|p| p.key == row.path || (!p.profile.is_empty() && p.profile == row.profile_path))
            && !row.active
        {
            row.state = 40;
        }
    }
}
async fn nm_owner(connection: &Connection) -> Option<String> {
    let bus = Proxy::new(
        connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    .ok()?;
    bus.call("GetNameOwner", &(NM,)).await.ok()
}
pub fn start() -> Service {
    start_with_connection(None)
}
fn start_with_connection(initial: Option<Connection>) -> Service {
    let (sender, updates) = watch::channel(Snapshot::default());
    let (commands, mut actions) = mpsc::channel(64);
    let (visible, mut visibility) = watch::channel(false);
    tokio::spawn(async move {
        let mut state = Snapshot::default();
        let mut connection: Option<Connection> = initial;
        let mut owner = String::new();
        let mut generation = 0u64;
        let mut signals: Option<zbus::MessageStream> = None;
        let control = agent::Control::new();
        if let Some(bus) = &connection {
            let _ = control.export(bus).await;
        }
        let mut credentials = control.updates.clone();
        let (snapshot_tx, mut snapshots) = mpsc::channel(2);
        let (complete_tx, mut completed) = mpsc::channel::<Completed>(16);
        let mut loading = false;
        let mut dirty = true;
        let mut due = Instant::now();
        let mut owner_check = Instant::now() - Duration::from_secs(4);
        let mut registration_retry = Instant::now();
        let mut registered = false;
        let mut next_id = 0u64;
        let mut jobs = HashMap::<u64, Job>::new();
        let mut cancelled_jobs = HashSet::<u64>::new();
        let mut snapshot_job: Option<tokio::task::JoinHandle<()>> = None;
        let mut pending = HashMap::<u64, Pending>::new();
        let mut scans = HashMap::<String, (i64, Instant)>::new();
        let mut operation_error = String::new();
        let (traffic_tx, mut traffic_updates) = mpsc::channel(1);
        let mut traffic_loading = false;
        let mut traffic_due = Instant::now();
        let mut traffic_sampler = traffic::Sampler::default();
        let mut traffic_generation = 0u64;
        let mut traffic_received = Instant::now();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _=tick.tick()=>{
                    if state.traffic.ready && traffic_received.elapsed()>Duration::from_secs(3) {
                        state.traffic.ready=false;
                    }
                    if !traffic_loading && *visibility.borrow() && Instant::now()>=traffic_due {
                        traffic_due=Instant::now()+Duration::from_secs(1);
                        if let Some(iface)=traffic::interface(&state) {
                            traffic_loading=true;let tx=traffic_tx.clone();let epoch=generation;let sample_epoch=traffic_generation;
                            tokio::spawn(async move { let result=tokio::task::spawn_blocking({let iface=iface.clone();move||traffic::read(std::path::Path::new("/sys/class/net"),&iface).ok()}).await.ok().flatten();let _=tx.send((epoch,sample_epoch,iface,result)).await; });
                        } else { traffic_sampler.reset();state.traffic=Traffic::default(); }
                    }
                    if owner_check.elapsed()>=Duration::from_secs(3)||connection.is_none(){
                        owner_check=Instant::now();
                        let mut bus_changed = false;
                        if connection.as_ref().is_some_and(Connection::is_closed){connection=None;bus_changed=true;}
                        if connection.is_none(){
                            if let Ok(Ok(bus))=tokio::time::timeout(Duration::from_secs(2),Connection::system()).await{
                                let _=control.export(&bus).await;connection=Some(bus);
                            }
                        }
                        let found=if let Some(bus)=&connection{tokio::time::timeout(Duration::from_secs(1),nm_owner(bus)).await.ok().flatten()}else{None};
                        let found=found.unwrap_or_default();
                        if found.is_empty()&&owner.is_empty()&&!state.available&&state.error.is_empty(){state=unavailable("NetworkManager is unavailable".into());}
                        if found!=owner || bus_changed{
                            owner=found;generation=generation.wrapping_add(1);registered=false;control.clear();signals=None;
                            for(_,job)in jobs.drain(){job.handle.abort();}cancelled_jobs.clear();pending.clear();scans.clear();operation_error.clear();
                            if let Some(job)=snapshot_job.take(){job.abort();}loading=false;
                            dirty=true;due=Instant::now();
                            if owner.is_empty(){state=unavailable("NetworkManager is unavailable".into());sender.send_replace(state.clone());}
                            else if let Some(bus)=&connection{
                                let rule=zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(owner.as_str()).unwrap().path_namespace(ROOT).unwrap().build();
                                signals=zbus::MessageStream::for_match_rule(rule,bus,Some(128)).await.ok();
                            }
                        }
                    }
                    if !registered&&!owner.is_empty()&&registration_retry.elapsed()>=Duration::from_secs(2){
                        registration_retry=Instant::now();if let Some(bus)=&connection{registered=matches!(tokio::time::timeout(Duration::from_secs(2),control.register(bus)).await,Ok(Ok(())));}
                        state.secret_agent=registered;
                    }
                    if !owner.is_empty()&&!loading&&Instant::now()>=due{
                        loading=true;dirty=false;let bus=connection.as_ref().unwrap().clone();let tx=snapshot_tx.clone();let epoch=generation;
                        snapshot_job=Some(tokio::spawn(async move{let result=match tokio::time::timeout(Duration::from_secs(8),snapshot(&bus)).await{Ok(result)=>result,Err(_)=>Err(anyhow::anyhow!("NetworkManager is not responding"))};let _=tx.send((epoch,result)).await;}));
                    }
                    let expired:Vec<_>=pending.iter().filter(|(_,p)|p.started.elapsed()>Duration::from_secs(90)).map(|(id,_)|*id).collect();
                    for id in expired{pending.remove(&id);operation_error="The connection timed out. Check the network credentials and availability".into();finish_operation(&mut state,id,&operation_error);}
                    scans.retain(|_,(_,time)|time.elapsed()<Duration::from_secs(25));
                    state.scanning = !scans.is_empty();for device in &mut state.devices{device.scanning=scans.contains_key(&device.path);}
                    overlay_pending(&mut state,&pending);state.credentials=credentials.borrow().clone();state.secret_agent=registered;
                    sender.send_if_modified(|old|{if *old==state{false}else{*old=state.clone();true}});
                }
                signal=async{match &mut signals{Some(s)=>s.next().await,None=>std::future::pending().await}}=>{
                    if matches!(signal,Some(Ok(_))){dirty=true;due=due.min(Instant::now()+Duration::from_millis(180));}else{signals=None;owner_check=Instant::now()-Duration::from_secs(4);}
                }
                result=snapshots.recv()=>{
                    if let Some((epoch,result))=result{if epoch!=generation{continue}loading=false;snapshot_job.take();
                        let old_active:Vec<_>=state.activations.iter().filter(|a|a.state==2).map(|a|a.path.clone()).collect();
                        let (id,kind,target,done,error)=(state.operation_id,state.operation_kind.clone(),state.operation_target.clone(),state.operation_done,state.operation_error.clone());
                        let current_traffic=state.traffic.clone();
                        state=match result{Ok(s)=>s,Err(e)=>unavailable(action_error(&e))};
                        if traffic::interface(&state).as_deref()==Some(current_traffic.device.as_str()){state.traffic=current_traffic;}
                        state.operation_id=id;state.operation_kind=kind;state.operation_target=target;state.operation_done=done;state.operation_error=error;
                        due=Instant::now()+if dirty{Duration::from_millis(180)}else{Duration::from_secs(if *visibility.borrow(){2}else{20})};
                        let mut finished=Vec::new();
                        for(id,p)in &pending{
                            let is_already_active = state.networks.iter().any(|n| {
                                (n.path == p.key || (!p.profile.is_empty() && (n.profile_path == p.profile || n.saved == p.profile))) && n.active
                            });
                            if is_already_active {
                                finished.push((*id, String::new()));
                                continue;
                            }
                            if p.active.is_empty(){continue}
                            let active=state.activations.iter().find(|a|a.path==p.active);
                            match active{
                                Some(active) if active.state==2=>{finished.push((*id,String::new()));}
                                Some(active) if active.state==4=>{
                                    let message=state.devices.iter().find(|d|d.path==p.device&&!d.error.is_empty()).map(|d|d.error.clone()).unwrap_or_else(||if active.reason==9{"The network password or credentials are missing".into()}else if active.reason==10{"Network authentication failed".into()}else{"The network connection failed".into()});finished.push((*id,message));
                                }
                                None if state.available&&p.started.elapsed()>Duration::from_secs(2)=>{finished.push((*id,"The network connection ended before it became active".into()));}
                                _=>{}
                            }
                        }
                        for(id,error)in finished{pending.remove(&id);operation_error=error.clone();finish_operation(&mut state,id,&error);}

                        if pending.is_empty()&&state.activations.iter().any(|a|a.state==2&&!old_active.contains(&a.path)){operation_error.clear();}
                        if state.error.is_empty(){state.error=operation_error.clone();}
                        scans.retain(|path,(before,_)|!state.devices.iter().any(|device|&device.path==path&&device.last_scan>*before));
                        overlay_pending(&mut state,&pending);state.credentials=credentials.borrow().clone();state.secret_agent=registered;sender.send_replace(state.clone());
                    }
                }
                result=traffic_updates.recv()=>{
                    if let Some((epoch,sample_epoch,iface,counters))=result { traffic_loading=false;if epoch==generation&&sample_epoch==traffic_generation&&*visibility.borrow()&&traffic::interface(&state).as_deref()==Some(iface.as_str()) { state.traffic=traffic_sampler.update(iface,counters);traffic_received=Instant::now();sender.send_replace(state.clone()); }else{traffic_sampler.reset();} }
                }
                result=completed.recv()=>{if let Some(result)=result{let origin=jobs.remove(&result.id);let editor=origin.as_ref().is_some_and(|job|job.editor);let wireless=origin.as_ref().is_some_and(|job|job.wireless);if result.generation!=generation{continue}
                    if cancelled_jobs.remove(&result.id){

                        if let Ok(applied)=result.result{if !applied.active.is_empty(){
                            let bus=connection.as_ref().unwrap().clone();
                            tokio::spawn(async move{let _=tokio::time::timeout(Duration::from_secs(5),operations::apply(&bus,Action::CancelActive{active:vec![applied.active],devices:vec![]},&Snapshot::default())).await;});
                        }}
                        dirty=true;due=Instant::now();continue;
                    }
                    match result.result{
                        Ok(applied)=>{
                            operation_error.clear();
                            if !applied.active.is_empty(){pending.insert(result.id,Pending{key:result.target.clone(),active:applied.active,profile:applied.profile,device:applied.device,started:Instant::now(),operation:result.id,editor,wireless});}
                            else{finish_operation(&mut state,result.id,"");}
                            for(path,last)in applied.scans{scans.insert(path,(last,Instant::now()));}
                        }
                        Err(error)=>{pending.remove(&result.id);let error=action_error(&error);operation_error=error.clone();finish_operation(&mut state,result.id,&error);}
                    }
                    dirty=true;due=Instant::now();sender.send_replace(state.clone());
                }}
                changed=credentials.changed()=>{if changed.is_ok(){state.credentials=credentials.borrow_and_update().clone();sender.send_replace(state.clone());}}
                changed=visibility.changed()=>{if changed.is_err(){break}dirty=true;due=Instant::now();traffic_due=Instant::now();traffic_generation=traffic_generation.wrapping_add(1);traffic_sampler.reset();state.traffic=Traffic::default();}
                action=actions.recv()=>{
                    let Some(mut action)=action else{break};
                    match action{
                        Action::ReplySecrets{id,username,password,private_key_password,extra,cancel}=>{if let Err(error)=control.reply(id,username,password,private_key_password,extra,cancel){state.error=action_error(&error);}sender.send_replace(state.clone());continue}
                        Action::ResetError=>{operation_error.clear();state.error.clear();state.operation_error.clear();sender.send_replace(state.clone());continue}
                        _=>{}
                    }
                    let(kind,target)=operations::classify(&action);next_id=next_id.wrapping_add(1);let id=next_id;mark_operation(&mut state,id,kind,&target);
                    if matches!(&action,Action::Wifi(false)|Action::Networking(false)|Action::Disconnect(_)|Action::Cancel(_)|Action::Forget(_)){
                        let all_off=matches!(&action,Action::Networking(false));
                        let wifi_off=matches!(&action,Action::Wifi(false));
                        let editor_cancel=matches!(&action,Action::Cancel(path) if path=="profile-editor");
                        let matches_pending=|p:&Pending|all_off||(wifi_off&&p.wireless)||(editor_cancel&&p.editor)||(!editor_cancel&&!target.is_empty()&&(p.key==target||p.profile==target||p.device==target));
                        let mut cancel_active=Vec::new();
                        for p in pending.values().filter(|p|matches_pending(p)){if !p.active.is_empty(){cancel_active.push(p.active.clone());}}
                        let removed:Vec<_>=pending.iter().filter(|(_,p)|matches_pending(p)).map(|(id,_)|*id).collect();
                        for id in removed{pending.remove(&id);}
                        for (id,job) in &jobs{if all_off||(wifi_off&&job.wireless)||(editor_cancel&&job.editor)||(!editor_cancel&&!target.is_empty()&&(job.key==target||job.profile==target||job.device==target)){cancelled_jobs.insert(*id);}}
                        if editor_cancel{action=Action::CancelActive{active:cancel_active,devices:vec![]};}
                        if wifi_off||all_off{scans.clear();}
                        if all_off{control.clear();}
                    }
                    if owner.is_empty()||connection.is_none(){operation_error="NetworkManager is unavailable. Start the service before changing network settings".into();finish_operation(&mut state,id,&operation_error);sender.send_replace(state.clone());continue}
                    if jobs.len()>=8{operation_error="A network operation is already in progress. Wait for it to finish".into();finish_operation(&mut state,id,&operation_error);sender.send_replace(state.clone());continue}
                    let (profile,device,wireless)=action_context(&action,&state);
                    if matches!(kind,"connect"|"activate"|"connect-profile"|"hidden"|"hotspot"){
                        pending.insert(id,Pending{key:target.clone(),active:String::new(),profile:profile.clone(),device:device.clone(),started:Instant::now(),operation:id,editor:operations::editor_kind(kind),wireless});overlay_pending(&mut state,&pending);
                    }
                    let bus=connection.as_ref().unwrap().clone();let previous=state.clone();let tx=complete_tx.clone();let epoch=generation;let key=target.clone();
                    let job=tokio::spawn(async move{let result=match tokio::time::timeout(Duration::from_secs(30),operations::apply(&bus,action,&previous)).await{Ok(result)=>result,Err(_)=>Err(anyhow::anyhow!("NetworkManager timed out. The operation may still be finishing"))};let _=tx.send(Completed{generation:epoch,id,target,result}).await;});
                    jobs.insert(id,Job{key,profile,device,editor:operations::editor_kind(kind),wireless,handle:job});sender.send_replace(state.clone());
                }
            }
        }
        for (_, job) in jobs {
            job.handle.abort();
        }
        if let Some(job) = snapshot_job {
            job.abort();
        }
        control.clear();
    });
    Service {
        updates,
        commands,
        visible,
    }
}

#[cfg(test)]
mod snapshot_tests;
#[cfg(test)]
mod tests;
