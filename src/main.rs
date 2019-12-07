use std::thread::{self, *};

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::marker::*;
use std::ops::Deref;
use std::sync::mpsc::{self, *};
use std::sync::RwLock;
use std::sync::*;
use std::time::*;

struct NodeThread {
    handle: JoinHandle<()>,
    main_thread_sender: EventSender,
    node_thread_sender: Sender<NodeThreadEvent>,
    am_i_busy: Arc<RwLock<bool>>,
    thread_id: usize,
}

// impl Drop for NodeThread {
//     fn drop(&mut self) {
//         println!("thread {} drop",self.thread_id);
//     }
// }

#[derive(Clone, Debug)]
enum Event {
    Stop,
    EpollTimeout,
    EpollComplete(usize),
    ThreadPoolComplete(usize, usize, JSType), // thread_id, complete callback id return val
}

enum NodeThreadEvent {
    Stop,
    Task(usize, DependLessCallBack),
}
unsafe impl Send for NodeThreadEvent {}
type EventSender = SenderWraper<Event>;
type EventReceiver = ReceverWraper<Event>;

impl NodeThread {
    fn new(thread_id: usize, n2ms: EventSender) -> Self {
        let (m2n_s, m2n_r) = mpsc::channel();
        let main_thread_sender_clone = n2ms.clone();
        let thread_id_clone = thread_id;
        let am_i_busy = Arc::new(RwLock::new(false));
        let am_i_busy_clone = am_i_busy.clone();
        let handle = spawn(move || {
            println!("node thread {} start", thread_id);
            loop {
                match m2n_r.recv() {
                    Ok(NodeThreadEvent::Stop) => {
                        break;
                    }
                    Ok(NodeThreadEvent::Task(id, callback)) => {
                        println!(" thread {} get a job {}", thread_id, id);
                        {
                            let mut am_i_busy = am_i_busy_clone.write().unwrap();
                            *am_i_busy = true;
                        }
                        let res = callback();
                        main_thread_sender_clone
                            .send(Event::ThreadPoolComplete(thread_id, id, res));
                        {
                            let mut am_i_busy = am_i_busy_clone.write().unwrap();
                            *am_i_busy = false;
                        }
                    }
                    Err(err) => {
                        println!("thread {} error {:?}", thread_id, err);
                    }
                }
            }
        });
        println!("thread new {}", thread_id);
        return Self {
            handle,
            thread_id,
            main_thread_sender: n2ms,
            node_thread_sender: m2n_s,
            am_i_busy,
        };
    }

    fn is_busy(&mut self) -> bool {
        return *self.am_i_busy.read().unwrap();
    }

    fn shut_down(&mut self) {
        self.node_thread_sender.send(NodeThreadEvent::Stop);
    }

    fn wait(self) {
        self.handle.join();
    }

    fn run_task(&mut self, id: usize, cb: DependLessCallBack) {
        let res = self.node_thread_sender.send(NodeThreadEvent::Task(id, cb));
        if let Err(e) = res {
            println!("run task {} fail {}", id, e.description());
        }
    }
    fn get_thread_id(&self) -> usize {
        return self.thread_id;
    }
}

#[derive(Clone, Debug)]
enum JSType {
    JInt(isize),
    JString(String),
    JVoid,
}

type CallBack = Box<dyn FnOnce(JSType) -> JSType>;
type DependLessCallBack = Box<dyn FnOnce() -> JSType>;

struct Fs {}

impl Fs {
    pub fn read<F>(path: &str, callback: F)
    where
        F: 'static + FnOnce(JSType) -> JSType,
    {
        let path = path.to_string();
        let real_read_fn = || {
            println!("p {:?} {:?}", path, std::env::current_dir());
            let data = std::fs::read_to_string(path).unwrap();
            return JSType::JString(data);
        };

        let r: &mut Runtime = unsafe { &mut *RUNTIME };
        let id = r.generate_callback_id();
        r.add_to_ready_to_run_queue(id, Box::new(real_read_fn));
        r.add_to_wait_callback_queue(id, Box::new(callback));
    }
}

struct Http {}

impl Http {
    fn get<F>(host: &str, url: &str, cb: F)
    where
        F: 'static + FnOnce(JSType) -> JSType,
    {
        use std::io::*;
        let rt: &mut Runtime = unsafe { &mut *RUNTIME };
        let mut stream = minimio::TcpStream::connect(format!("{}:80", host)).unwrap();
        let request = format!(
            "GET {} HTTP/1.1\r\n\
             Host: {}\r\n\
             Connection: close\r\n\
             \r\n",
            url, host
        );
        stream
            .write_all(request.as_bytes())
            .expect("Error writing to stream");

        let token = rt.generate_callback_id();
        rt.epoll_thread
            .registrator
            .register(&mut stream, token, minimio::Interests::readable())
            .unwrap();

        let wrapper = |_: JSType| {
            let mut stream = stream;
            let mut buffer = String::new();
            stream.read_to_string(&mut buffer).unwrap();
            cb(JSType::JString(buffer))
        };
        rt.add_to_wait_callback_queue(token, Box::new(wrapper));
    }
}

struct EpollThread {
    main_thread_sender: EventSender,
    handle: JoinHandle<()>,
    timeout: Arc<Mutex<Option<Duration>>>,
    registrator: minimio::Registrator,
}
impl EpollThread {
    fn new(sender: EventSender) -> Self {
        let timeout = Arc::new(Mutex::new(Some(Duration::from_secs(3))));
        let timeout_clone = timeout.clone();
        let mut poll = minimio::Poll::new().expect("create poll fail");
        let registrator = poll.registrator();
        let sender_clone = sender.clone();
        let handle = thread::spawn(move || {
            let mut events = minimio::Events::with_capacity(1024);
            loop {
                let epoll_timeout = {
                    let epoll_timeout = timeout_clone.lock().unwrap();
                    *epoll_timeout
                };
                let epoll_timeout = epoll_timeout.map(|d: Duration| d.as_millis() as i32);
                println!(
                    "=======>set epoll timeout {:?} {:?}",
                    epoll_timeout,
                    Instant::now()
                );
                match poll.poll(&mut events, epoll_timeout) {
                    Ok(v) if v > 0 => {
                        for i in 0..v {
                            println!("epoll event {}", i);
                            let event = events.get_mut(i).expect("get event fail");
                            sender_clone.send(Event::EpollComplete(event.id().value()));
                        }
                    }
                    Ok(v) if v == 0 => {
                        println!(
                            "=========> over epoll timeout {:?} {:?}",
                            epoll_timeout,
                            Instant::now()
                        );
                        sender_clone.send(Event::EpollTimeout);
                    }
                    Ok(_) => {
                        println!("????????????");
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => break,
                    Err(e) => {
                        println!("something error {:?}", e);
                    }
                }
            }
        });

        Self {
            main_thread_sender: sender,
            handle,
            timeout,
            registrator,
        }
    }
    fn set_timeout(&mut self, timeout: Duration) {
        let mut timeout_handle = self.timeout.lock().unwrap();
        *timeout_handle = Some(timeout);
        println!("set epoll timeout {:?}", timeout);
    }
}
struct Runtime {
    thread_pool: Vec<NodeThread>,
    epoll_thread: EpollThread,
    event_receiver: EventReceiver,
    ready_to_run_callback: Vec<(usize, DependLessCallBack)>, //存储着希望被立即执行的callback的信息
    id_base: usize,
    time_map: BTreeMap<Instant, usize>, // 里面存储这等待被调用的callback的id和期望被调用的时间
    wait_callback_map: HashMap<usize, Vec<usize>>, //存储等待某个callback的callback id
    wait_scheduce_callback: HashMap<usize, CallBack>, //存储着不知什么时候会被调用的回调
}

struct SenderWraper<T>(mpsc::Sender<T>);
struct ReceverWraper<T>(mpsc::Receiver<T>);

impl<T> Drop for SenderWraper<T> {
    fn drop(&mut self) {
        println!("i drop");
    }
}

impl<T> Drop for ReceverWraper<T> {
    fn drop(&mut self) {
        println!("i drop");
    }
}

impl<T> Deref for SenderWraper<T> {
    type Target = mpsc::Sender<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> Deref for ReceverWraper<T> {
    type Target = mpsc::Receiver<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

fn channel<T>() -> (SenderWraper<T>, ReceverWraper<T>) {
    let (s, r) = mpsc::channel::<T>();
    return (SenderWraper(s), ReceverWraper(r));
}

impl Runtime {
    fn new() -> Self {
        let (sender, receiver) = channel();
        // let thread_pool = (0..1).map(|id| NodeThread::new(id,SenderWraper(sender.clone()))).collect();
        let mut thread_pool = vec![NodeThread::new(1, SenderWraper(sender.clone()))];
        println!("thread_pool construct");
        let epoll_thread = EpollThread::new(sender);
        // let res = unsafe{ thread_pool.get_unchecked_mut(0).node_thread_sender.send(NodeThreadEvent::Stop)};
        // println!("res {:?}",res);
        return Self {
            thread_pool,
            epoll_thread,
            time_map: Default::default(),
            event_receiver: receiver,
            ready_to_run_callback: vec![],
            id_base: 0,
            wait_callback_map: Default::default(),
            wait_scheduce_callback: Default::default(),
        };
    }

    fn am_i_busy(&self) -> bool {
        println!(
            "am_i_busy {} {}",
            self.wait_scheduce_callback.len(),
            self.ready_to_run_callback.len()
        );
        let i_free_now =
            self.wait_scheduce_callback.is_empty() && self.ready_to_run_callback.is_empty();
        return !i_free_now;
    }

    fn dispatch_timer_to_ready_callback(&mut self) {
        let not_yet_list = self.time_map.split_off(&Instant::now());
        println!("dispatch_timer_to_ready_callback {:?}", not_yet_list);
        // let ids = {self.time_map.values().clone()};

        let ids = self.time_map.clone();
        self.time_map = not_yet_list;

        for (_, id) in ids {
            self.dispatch_wait_callback_to_ready_callback(id, JSType::JVoid)
        }
    }

    fn dispatch_ready_callback_to_avaiable_thread(&mut self) {
        for t in self.thread_pool.iter_mut() {
            if !t.is_busy() {
                if let Some((id, cb)) = self.ready_to_run_callback.pop() {
                    println!("dispatch {} to thread {}", id, t.get_thread_id());
                    t.run_task(id, cb);
                }
            }
        }
    }

    fn get_next_timer(&self) -> Duration {
        if let Some(instant) = self.time_map.keys().next() {
            let now = Instant::now();
            println!("{:?} {:?}", now, instant);
            if *instant > now {
                return *instant - now;
            }
            // 已经超时了
            return Duration::from_secs(1);
        }
        return Duration::from_secs(5);
    }

    fn dispatch_wait_callback_to_ready_callback(
        &mut self,
        comlpete_callback_id: usize,
        ret_val: JSType,
    ) {
        let ids = if let Some(ids) = self.wait_callback_map.remove(&comlpete_callback_id) {
            ids
        } else {
            return;
        };

        for id in ids {
            if let Some(cb) = self.wait_scheduce_callback.remove(&id) {
                println!("dispatch_wait_callback_to_ready_callback {}", id);
                let ret_val_clone = ret_val.clone();
                let depend_less_cb = Box::new(move || cb(ret_val_clone));
                self.ready_to_run_callback.push((id, depend_less_cb));
            }
        }
    }

    fn start(mut self, f: impl Fn()) {
        println!("start");
        let rt_ptr: *mut Runtime = &mut self;
        unsafe { RUNTIME = rt_ptr };
        f();
        while self.am_i_busy() {
            //is there sth it need to do still?
            println!("in loop");
            self.dispatch_timer_to_ready_callback();
            self.dispatch_ready_callback_to_avaiable_thread();
            let timer = self.get_next_timer();
            println!("next timer is {:?}", timer);
            self.epoll_thread.set_timeout(timer); //使用这种timeout的副作用来完成 这样 在receive时就能收到刚刚好的EpollTimeout了
            println!("{:?}", self.event_receiver.try_recv());
            if let Ok(event) = self.event_receiver.recv() {
                println!("get event");
                match event {
                    Event::Stop => {
                        //???
                    }
                    Event::EpollTimeout => {
                        //nothing to do the next loop dispatch_timer_to_ready_callback will do what we want
                    }
                    Event::EpollComplete(id) => {
                        self.dispatch_wait_callback_to_ready_callback(id, JSType::JVoid);
                    }
                    Event::ThreadPoolComplete(thread_id, comlpete_callback_id, return_val) => {
                        println!("ThreadPoolComplete {} {} ", thread_id, comlpete_callback_id);
                        self.dispatch_wait_callback_to_ready_callback(
                            comlpete_callback_id,
                            return_val,
                        );
                    }
                }
            }
        }
        println!("all over");
        for mut t in self.thread_pool.into_iter() {
            t.shut_down();
            t.wait();
        }
    }

    //当id callback发生完成之后调用callback
    fn add_to_wait_callback_queue(&mut self, id: usize, callback: CallBack) {
        let cb_id = self.generate_callback_id();
        println!("wait_scheduce_callback.insert {}", id);
        self.wait_scheduce_callback.insert(cb_id, callback);
        self.wait_callback_map
            .entry(id)
            .or_insert(vec![id])
            .push(cb_id);
    }

    fn add_to_ready_to_run_queue(&mut self, id: usize, callback: DependLessCallBack) {
        self.ready_to_run_callback.push((id, callback));
    }

    fn generate_callback_id(&mut self) -> usize {
        self.id_base += 1;
        return self.id_base;
    }

    fn set_timeout<F>(&mut self, callback: F, timeout: usize)
    where
        F: 'static + FnOnce(JSType) -> JSType,
    {
        let callback = Box::new(callback);
        let id = self.generate_callback_id(); //这个callback 是tineout事件的id
        println!("timeout id {:?}", id);
        self.add_to_wait_callback_queue(id, callback);
        let dur = Duration::from_millis(timeout as u64);
        let instant = Instant::now() + dur;
        self.time_map.insert(instant, id);
    }
}

fn set_timeout<F>(callback: F, timeout: usize)
where
    F: 'static + FnOnce(JSType) -> JSType,
{
    let r: &mut Runtime = unsafe { &mut *RUNTIME };
    r.set_timeout(callback, timeout)
}

static mut RUNTIME: *mut Runtime = std::ptr::null_mut();

//js like function
fn run() {
    set_timeout(
        |_| {
            println!("=====================> timeout1 {:?}", Instant::now());
            set_timeout(
                |_| {
                    println!("=====================> timeout2 {:?}", Instant::now());
                    JSType::JVoid
                },
                1000 * 5,
            );
            JSType::JVoid
        },
        1000 * 5,
    );

    Http::get("baidu.com", "/sddsdsds", |data| {
        println!("data {:?}", data);
        JSType::JVoid
    });

    Fs::read("./Cargo.toml", |data| {
        println!("data {:?}", data);
        return JSType::JVoid;
    })
}
fn main() {
    Runtime::new().start(run);
    println!("all over");
}
