use std::{
    sync::{Mutex, MutexGuard},
    thread::{self, Thread},
};

pub struct Executor<S> {
    state: Mutex<S>,
    stateful_list: Mutex<Vec<Box<dyn FnOnce(&mut S)>>>,
    // stateless list
    park_list: Mutex<Vec<Thread>>,
}

pub enum Work<'a, S> {
    Stateful(Box<dyn FnOnce(&mut S)>, MutexGuard<'a, S>),
    Stateless(Box<dyn FnOnce()>),
}

impl<S> Executor<S> {
    pub fn new(state: S) -> Self {
        Self {
            state: Mutex::new(state),
            stateful_list: Mutex::new(Vec::new()),
            park_list: Mutex::new(Vec::new()),
        }
    }

    pub fn submit_stateful(&self, task: impl FnOnce(&mut S) + 'static) {
        self.stateful_list.lock().unwrap().push(Box::new(task));
        if let Some(thread) = self.park_list.lock().unwrap().pop() {
            thread.unpark();
        }
    }

    pub fn steal_with_state<'a>(&self, state: MutexGuard<'a, S>) -> Work<'a, S> {
        loop {
            if let Some(task) = self.stateful_list.lock().unwrap().pop() {
                return Work::Stateful(task, state);
            }
            // try steal stateless task
            self.park_list.lock().unwrap().push(thread::current());
            thread::park();
        }
    }

    pub fn steal_without_state(&self) -> Work<'_, S> {
        loop {
            // try steal stateless task
            if let Ok(state) = self.state.try_lock() {
                if let Some(task) = self.stateful_list.lock().unwrap().pop() {
                    return Work::Stateful(task, state);
                }
            }
            self.park_list.lock().unwrap().push(thread::current());
            thread::park();
        }
    }
}
