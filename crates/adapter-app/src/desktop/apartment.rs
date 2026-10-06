use std::marker::PhantomData;
use std::rc::Rc;

pub(super) const CHANGED_MODE: i32 = 0x80010106_u32 as i32;

pub(super) trait Api {
    fn initialize_mta(&self) -> i32;
    fn uninitialize(&self);
}

pub(super) struct Guard<'a, A: Api> {
    api: &'a A,
    _same_thread: PhantomData<Rc<()>>,
}

impl<'a, A: Api> Guard<'a, A> {
    pub(super) fn enter(api: &'a A) -> std::result::Result<Self, i32> {
        let status = api.initialize_mta();
        if status < 0 {
            return Err(status);
        }
        // Both S_OK and S_FALSE own an initialization reference. The
        // non-Send marker keeps cleanup on the initializing thread.
        Ok(Self {
            api,
            _same_thread: PhantomData,
        })
    }
}

impl<A: Api> Drop for Guard<'_, A> {
    fn drop(&mut self) {
        self.api.uninitialize();
    }
}
