use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
        gles::{GlesError, GlesFrame, GlesRenderer},
        utils::{CommitCounter, DamageSet, OpaqueRegions},
    },
    utils::{Buffer, Physical, Rectangle, Scale, Transform},
};
use std::{cell::Cell, rc::Rc};

const DEFAULT_LIMIT: u64 = u64::MAX;

#[derive(Debug)]
struct Usage {
    bytes: Cell<u64>,
    limit: u64,
}
#[derive(Debug, Clone)]
pub(super) struct Budget(Rc<Usage>);
impl Default for Budget {
    fn default() -> Self {
        Self::new(DEFAULT_LIMIT)
    }
}
impl Budget {
    fn new(limit: u64) -> Self {
        Self(Rc::new(Usage {
            bytes: Cell::new(0),
            limit,
        }))
    }
    pub(super) fn reserve(&self, bytes: u64) -> Option<Rc<Lease>> {
        let total = self.0.bytes.get().checked_add(bytes)?;
        if total > self.0.limit {
            return None;
        }
        self.0.bytes.set(total);
        Some(Rc::new(Lease {
            usage: self.0.clone(),
            bytes,
        }))
    }
    pub(super) fn used(&self) -> u64 {
        self.0.bytes.get()
    }
    pub(super) fn limit(&self) -> u64 {
        self.0.limit
    }
    #[cfg(test)]
    pub(super) fn with_limit(limit: u64) -> Self {
        Self::new(limit)
    }
}

#[derive(Debug)]
pub(super) struct Lease {
    usage: Rc<Usage>,
    bytes: u64,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.usage.bytes.set(self.usage.bytes.get() - self.bytes);
    }
}

#[derive(Clone, Debug)]
pub struct Budgeted<E> {
    inner: E,
    id: Id,
    commit: CommitCounter,
    _lease: Rc<Lease>,
}
impl<E> Budgeted<E> {
    pub(super) fn new(inner: E, id: Id, commit: CommitCounter, lease: Rc<Lease>) -> Self {
        Self {
            inner,
            id,
            commit,
            _lease: lease,
        }
    }
}
impl<E: Element> Element for Budgeted<E> {
    fn id(&self) -> &Id {
        &self.id
    }
    fn current_commit(&self) -> CommitCounter {
        self.commit
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }
    fn transform(&self) -> Transform {
        self.inner.transform()
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }
    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {

        if commit == Some(self.commit) {
            DamageSet::default()
        } else {
            DamageSet::from_slice(&[Rectangle::from_size(self.geometry(scale).size)])
        }
    }
    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner.opaque_regions(scale)
    }
    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }
    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}
impl<E: RenderElement<GlesRenderer>> RenderElement<GlesRenderer> for Budgeted<E> {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        self.inner.draw(frame, src, dst, damage, opaque)
    }
    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        self.inner.underlying_storage(renderer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_capture_and_inflight_frames_share_the_same_bound() {
        let display = Budget::new(100);
        let capture = display.clone();
        let cache = display.reserve(60).unwrap();
        let frame = cache.clone();
        drop(cache);
        assert!(capture.reserve(41).is_none());
        let scratch = capture.reserve(40).unwrap();
        assert_eq!(display.used(), 100);
        drop(frame);
        assert_eq!(display.used(), 40);
        let another = display.reserve(60).unwrap();
        assert!(capture.reserve(1).is_none());
        drop(another);
        drop(scratch);
        assert_eq!(display.used(), 0);
    }

    #[test]
    fn oversize_and_overflow_requests_leave_accounting_unchanged() {
        let budget = Budget::new(64);
        assert!(budget.reserve(u64::MAX).is_none());
        let image = budget.reserve(64).unwrap();
        assert!(budget.reserve(u64::MAX).is_none());
        assert_eq!(budget.used(), 64);
        drop(image);
        assert_eq!(budget.used(), 0);
    }
}
