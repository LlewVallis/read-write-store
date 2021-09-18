#[cfg(loom)]
pub struct ConcurrentQueue<Element> {
    inner: loom::sync::Mutex<Vec<Element>>,
}

#[cfg(loom)]
impl<Element> ConcurrentQueue<Element> {
    pub fn new() -> Self {
        Self {
            inner: loom::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn push(&self, value: Element) {
        self.inner.lock().unwrap().insert(0, value)
    }

    pub fn pop(&self) -> Option<Element> {
        self.inner.lock().unwrap().pop()
    }
}

#[cfg(not(loom))]
pub struct ConcurrentQueue<Element> {
    inner: crossbeam_queue::SegQueue<Element>,
}

#[cfg(not(loom))]
impl<Element> ConcurrentQueue<Element> {
    pub fn new() -> Self {
        Self {
            inner: crossbeam_queue::SegQueue::new(),
        }
    }

    pub fn push(&self, value: Element) {
        self.inner.push(value)
    }

    pub fn pop(&self) -> Option<Element> {
        self.inner.pop()
    }
}
