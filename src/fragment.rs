use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet, LinkedList};
use std::hash::{BuildHasher, Hash};
use std::iter;
use std::marker::PhantomData;
use std::mem;
use std::sync::Arc;

use either::Either;
use failure::Error;
use parking_lot::Mutex;

use crate::extension::{Extensible, Extension};
use crate::validation::{Result as ValidationResult, Results as ValidationResults};

// TODO: Add logging/trace logs?
// TODO: Use ValidationResult instead?

#[derive(Debug)]
pub struct IdGen(u128);

impl IdGen {
    fn new() -> Self {
        IdGen(1)
    }
}

impl Default for IdGen {
    fn default() -> Self {
        Self::new()
    }
}

impl Iterator for IdGen {
    type Item = CacheId;
    fn next(&mut self) -> Option<CacheId> {
        let id = self.0;
        self.0 = self
            .0
            .checked_add(1)
            .expect("WTF? Run out of 128bit cache IDs!?");
        Some(CacheId(id))
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct CacheId(u128);

impl CacheId {
    fn dummy() -> Self {
        CacheId(0)
    }
}

pub enum CacheInstruction<Resource> {
    DropAll,
    DropSpecific(CacheId),
    Install { id: CacheId, resource: Resource },
}

pub trait Driver<F: Fragment> {
    type SubFragment: Fragment;
    fn instructions<T, I>(
        &mut self,
        fragment: &F,
        transform: &mut T,
        name: &str,
    ) -> Result<Vec<CacheInstruction<T::OutputResource>>, Vec<Error>>
    where
        T: Transformation<<Self::SubFragment as Fragment>::Resource, I, Self::SubFragment>;
    fn confirm(&mut self);
    fn abort(&mut self);
    fn maybe_cached(&self, frament: &F) -> bool;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TrivialDriver;

impl<F: Fragment> Driver<F> for TrivialDriver {
    type SubFragment = F;
    fn instructions<T, I>(
        &mut self,
        fragment: &F,
        transform: &mut T,
        name: &str,
    ) -> Result<Vec<CacheInstruction<T::OutputResource>>, Vec<Error>>
    where
        T: Transformation<F::Resource, I, F>,
    {
        let resource = fragment
            .create(name)
            .and_then(|r| transform.transform(r, fragment, name))
            .map_err(|e| vec![e])?;
        Ok(vec![
            CacheInstruction::DropAll,
            CacheInstruction::Install {
                id: CacheId::dummy(),
                resource,
            },
        ])
    }
    fn confirm(&mut self) {}
    fn abort(&mut self) {}
    fn maybe_cached(&self, _: &F) -> bool {
        false
    }
}

#[derive(Clone, Debug, Default)]
// TODO: Use some kind of immutable/persistent data structures? Or not, this is likely to be small?
pub struct IdMapping {
    mapping: HashMap<CacheId, CacheId>,
}

impl IdMapping {
    pub fn translate<'a, R, I>(&'a mut self, id_gen: &'a mut IdGen, instructions: I)
        -> impl Iterator<Item = CacheInstruction<R>> + 'a
    where
        R: 'a,
        I: IntoIterator<Item = CacheInstruction<R>> + 'a,
    {
        instructions
            .into_iter()
            // Borrow checker notes:
            // We need to move the self and id_gen into the closure. Otherwise, it creates
            // &mut &mut IdGen monster behind the scenes, however, the outer &mut has a short
            // lifetime because it points to the function's parameter.
            //
            // The mem::swap with the HashMap below is also because of borrow checker. The drain we
            // would like to use instead would have to eat the `&mut self` (or borrow it, but see
            // the same problem above). We *know* that we won't call this again until the drain
            // iterator is wholly consumed by flat_map, but the borrow checker doesn't. So this
            // trick instead.
            .flat_map(move |i| match i {
                CacheInstruction::DropAll => {
                    let mut mapping = HashMap::new();
                    mem::swap(&mut mapping, &mut self.mapping);
                    Either::Left(mapping
                        .into_iter()
                        .map(|(_, outer_id)| CacheInstruction::DropSpecific(outer_id)))
                },
                CacheInstruction::DropSpecific(id) => {
                    let id = self.mapping
                        .remove(&id)
                        .expect("Inconsistent use of cache: missing ID to remove");
                    Either::Right(iter::once(CacheInstruction::DropSpecific(id)))
                }
                CacheInstruction::Install { id, resource } => {
                    let new_id = id_gen.next().expect("Run out of cache IDs? Impossible");
                    assert!(self.mapping.insert(id, new_id).is_none(), "Duplicate ID created");
                    Either::Right(iter::once(CacheInstruction::Install { id: new_id, resource }))
                }
            })
    }
}

#[derive(Debug, Default)]
struct ItemDriver<Driver> {
    driver: Driver,
    id_mapping: IdMapping,
    proposed_mapping: Option<IdMapping>,
    used: bool,
    new: bool,
}

#[derive(Debug)]
pub struct SeqDriver<Item, SlaveDriver> {
    id_gen: IdGen,
    sub_drivers: Vec<ItemDriver<SlaveDriver>>,
    transaction_open: bool,
    // TODO: Can we actually get rid of this?
    _item: PhantomData<Fn(&Item)>,
}

// The derived Default balks on Item: !Default, but we *don't* need that
impl<Item, SlaveDriver> Default for SeqDriver<Item, SlaveDriver> {
    fn default() -> Self {
        Self {
            id_gen: IdGen::new(),
            sub_drivers: Vec::new(),
            transaction_open: false,
            _item: PhantomData,
        }
    }
}

// TODO: This one is complex enough, this calls for bunch of trace and debug logging!
impl<F, I, SlaveDriver> Driver<F> for SeqDriver<I, SlaveDriver>
where
    F: Fragment,
    // TODO: This could be generalized to ToOwned, right?
    I: Fragment,
    for<'a> &'a F: IntoIterator<Item = &'a I>,
    SlaveDriver: Driver<I> + Default,
{
    type SubFragment = SlaveDriver::SubFragment;
    fn instructions<T, Ins>(
        &mut self,
        fragment: &F,
        transform: &mut T,
        name: &str,
    ) -> Result<Vec<CacheInstruction<T::OutputResource>>, Vec<Error>>
    where
        T: Transformation<<Self::SubFragment as Fragment>::Resource, Ins, Self::SubFragment>,
    {
        assert!(!self.transaction_open);
        self.transaction_open = true;
        let mut instructions = Vec::new();
        let mut errors = Vec::new();

        for sub in fragment {
            let existing = self
                .sub_drivers
                .iter_mut()
                .find(|d| !d.used && d.driver.maybe_cached(sub));
            // unwrap_or_else angers the borrow checker here
            let slot = if let Some(existing) = existing {
                existing
            } else {
                self.sub_drivers.push(ItemDriver::default());
                let slot = self.sub_drivers.last_mut().unwrap();
                slot.new = true;
                slot
            };

            slot.used = true;
            match slot.driver.instructions(sub, transform, name) {
                Ok(new_instructions) => {
                    let mapping = if slot.new {
                        &mut slot.id_mapping
                    } else {
                        slot.proposed_mapping = Some(slot.id_mapping.clone());
                        slot.proposed_mapping.as_mut().unwrap()
                    };
                    instructions.extend(mapping.translate(&mut self.id_gen, new_instructions));
                }
                Err(errs) => errors.extend(errs),
            }
        }

        if errors.is_empty() {
            Ok(instructions)
        } else {
            self.abort();
            Err(errors)
        }
    }
    fn confirm(&mut self) {
        assert!(self.transaction_open);
        self.transaction_open = false;
        // Get rid of the unused ones
        self.sub_drivers.retain(|s| s.used);
        // Confirm all the used ones, accept proposed mappings and mark everything as old for next
        // round.
        for sub in &mut self.sub_drivers {
            sub.driver.confirm();
            if let Some(mapping) = sub.proposed_mapping.take() {
                sub.id_mapping = mapping;
            }
            sub.new = false;
        }
    }
    fn abort(&mut self) {
        assert!(self.transaction_open);
        self.transaction_open = false;
        // Get rid of the new ones completely
        self.sub_drivers.retain(|s| !s.new);
        // Abort anything we touched before
        for sub in &mut self.sub_drivers {
            if sub.used {
                sub.driver.abort();
                sub.proposed_mapping.take();
                sub.used = false;
            }
            assert!(
                sub.proposed_mapping.is_none(),
                "Proposed mapping for something not used"
            );
        }
    }
    fn maybe_cached(&self, fragment: &F) -> bool {
        fragment.into_iter().any(|s| {
            self.sub_drivers
                .iter()
                .any(|slave| slave.driver.maybe_cached(s))
        })
    }
}

pub trait Installer<Resource, O, C>: Default {
    type UninstallHandle: Send + 'static;
    fn install(&mut self, resource: Resource) -> Self::UninstallHandle;
    fn init<B: Extensible<Opts = O, Config = C>>(&mut self, builder: B) -> Result<B, Error> {
        Ok(builder)
    }
}

#[derive(Debug, Default)]
pub struct SeqInstaller<Slave> {
    slave: Slave,
}

impl<Resource, O, C, Slave> Installer<Resource, O, C> for SeqInstaller<Slave>
where
    Resource: IntoIterator,
    Slave: Installer<Resource::Item, O, C>,
{
    type UninstallHandle = Vec<Slave::UninstallHandle>;
    fn install(&mut self, resource: Resource) -> Self::UninstallHandle {
        resource
            .into_iter()
            .map(|r| self.slave.install(r))
            .collect()
    }
    fn init<B: Extensible<Opts = O, Config = C>>(&mut self, builder: B) -> Result<B, Error> {
        self.slave.init(builder)
    }
}

struct InstallCache<I, R, O, C>
where
    I: Installer<R, O, C>,
{
    installer: I,
    cache: HashMap<CacheId, I::UninstallHandle>,
    _type: PhantomData<(R, O, C)>,
}

impl<I, R, O, C> InstallCache<I, R, O, C>
where
    I: Installer<R, O, C>,
{
    fn new(installer: I) -> Self {
        Self {
            installer,
            cache: HashMap::new(),
            _type: PhantomData,
        }
    }
    fn interpret(&mut self, instruction: CacheInstruction<R>) {
        match instruction {
            CacheInstruction::DropAll => self.cache.clear(),
            CacheInstruction::DropSpecific(id) => assert!(self.cache.remove(&id).is_some()),
            CacheInstruction::Install { id, resource } => {
                let handle = self.installer.install(resource);
                assert!(self.cache.insert(id, handle).is_none());
            }
        }
    }
}

// Marker trait...
pub trait Stackable {}

pub trait Fragment: Sized {
    type Driver: Driver<Self> + Default;
    type Installer: Default;
    type Seed;
    type Resource;
    fn make_seed(&self, name: &str) -> Result<Self::Seed, Error>;
    fn make_resource(&self, seed: &mut Self::Seed, name: &str) -> Result<Self::Resource, Error>;
    fn create(&self, name: &str) -> Result<Self::Resource, Error> {
        let mut seed = self.make_seed(name)?;
        self.make_resource(&mut seed, name)
    }
}

// TODO: Export the macro for other containers?
macro_rules! fragment_for_seq {
    ($container: ident<$base: ident $(, $extra: ident)*> $(where $($bounds: tt)+)*) => {
        impl<$base: Clone + Fragment + Stackable + 'static $(, $extra)*> Fragment
            for $container<$base $(, $extra)*>
        $(
            where
            $($bounds)+
        )*
        {
            type Driver = SeqDriver<$base, $base::Driver>;
            type Installer = SeqInstaller<$base::Installer>;
            type Seed = Vec<$base::Seed>;
            type Resource = Vec<$base::Resource>;
            fn make_seed(&self, name: &str) -> Result<Self::Seed, Error> {
                self.iter().map(|i| i.make_seed(name)).collect()
            }
            fn make_resource(&self, seed: &mut Self::Seed, name: &str)
                -> Result<Self::Resource, Error>
            {
                self.iter()
                    .zip(seed)
                    .map(|(i, s)| i.make_resource(s, name))
                    .collect()
            }
        }
    }
}

fragment_for_seq!(Vec<T>);
fragment_for_seq!(BTreeSet<T>);
fragment_for_seq!(LinkedList<T>);
fragment_for_seq!(Option<T>);
fragment_for_seq!(BinaryHeap<T> where T: Ord);
fragment_for_seq!(HashSet<T, S> where T: Eq + Hash, S: BuildHasher);

// TODO: How do we stack maps, etc?
// TODO: Arcs, Rcs, Mutexes, refs, ...

// TODO: Make this into a macro instead, so we can impl Fragment for refs?
pub trait SimpleFragment: Sized {
    type SimpleResource;
    type SimpleInstaller: Default;
    fn make_simple_resource(&self, name: &str) -> Result<Self::SimpleResource, Error>;
}

impl<F: SimpleFragment> Fragment for F {
    type Driver = TrivialDriver;
    type Seed = ();
    type Installer = F::SimpleInstaller;
    type Resource = F::SimpleResource;
    fn make_seed(&self, _: &str) -> Result<(), Error> {
        Ok(())
    }
    fn make_resource(&self, _: &mut (), name: &str) -> Result<Self::Resource, Error> {
        self.make_simple_resource(name)
    }
}

// TODO: Allow returning refs somehow?
pub trait Extractor<O, C> {
    type Fragment: Fragment;
    fn extract(&mut self, opts: &O, config: &C) -> Self::Fragment;
}

impl<O, C, F, R> Extractor<O, C> for F
where
    F: FnMut(&O, &C) -> R,
    R: Fragment,
{
    type Fragment = R;
    fn extract(&mut self, opts: &O, config: &C) -> R {
        self(opts, config)
    }
}

pub struct CfgExtractor<F>(F);

impl<O, C, F, R> Extractor<O, C> for CfgExtractor<F>
where
    F: FnMut(&C) -> R,
    R: Fragment,
{
    type Fragment = R;
    fn extract(&mut self, _: &O, config: &C) -> R {
        (self.0)(config)
    }
}

pub trait Transformation<InputResource, InputInstaller, SubFragment> {
    type OutputResource;
    type OutputInstaller;
    fn installer(&mut self, installer: InputInstaller, name: &str) -> Self::OutputInstaller;
    fn transform(
        &mut self,
        resource: InputResource,
        fragment: &SubFragment,
        name: &str,
    ) -> Result<Self::OutputResource, Error>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NopTransformation;

impl<R, I, S> Transformation<R, I, S> for NopTransformation {
    type OutputResource = R;
    type OutputInstaller = I;
    fn installer(&mut self, installer: I, _: &str) -> I {
        installer
    }
    fn transform(&mut self, resource: R, _: &S, _: &str) -> Result<Self::OutputResource, Error> {
        Ok(resource)
    }
}

pub struct ChainedTransformation<A, B>(A, B);

impl<A, B, R, I, S> Transformation<R, I, S> for ChainedTransformation<A, B>
where
    A: Transformation<R, I, S>,
    B: Transformation<A::OutputResource, A::OutputInstaller, S>,
{
    type OutputResource = B::OutputResource;
    type OutputInstaller = B::OutputInstaller;
    fn installer(&mut self, installer: I, name: &str) -> B::OutputInstaller {
        let installer = self.0.installer(installer, name);
        self.1.installer(installer, name)
    }
    fn transform(
        &mut self,
        resource: R,
        fragment: &S,
        name: &str,
    ) -> Result<Self::OutputResource, Error> {
        let resource = self.0.transform(resource, fragment, name)?;
        self.1.transform(resource, fragment, name)
    }
}

pub struct SetInstaller<T, I>(T, Option<I>);

impl<T, I, R, OI, S> Transformation<R, OI, S> for SetInstaller<T, I>
where
    T: Transformation<R, OI, S>,
{
    type OutputResource = T::OutputResource;
    type OutputInstaller = I;
    fn installer(&mut self, _installer: OI, _: &str) -> I {
        self.1
            .take()
            .expect("SetInstaller::installer called more than once")
    }
    fn transform(
        &mut self,
        resource: R,
        fragment: &S,
        name: &str,
    ) -> Result<Self::OutputResource, Error> {
        self.0.transform(resource, fragment, name)
    }
}

pub struct Pipeline<Fragment, Extractor, Driver, Transformation> {
    name: &'static str,
    _fragment: PhantomData<Fn() -> Fragment>,
    extractor: Extractor,
    driver: Driver,
    transformation: Transformation,
}

impl Pipeline<(), (), (), ()> {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            _fragment: PhantomData,
            extractor: (),
            driver: (),
            transformation: (),
        }
    }

    pub fn extract<O, C, E: Extractor<O, C>>(
        self,
        e: E,
    ) -> Pipeline<E::Fragment, E, <E::Fragment as Fragment>::Driver, NopTransformation> {
        Pipeline {
            name: self.name,
            _fragment: PhantomData,
            extractor: e,
            driver: Default::default(),
            transformation: NopTransformation,
        }
    }

    pub fn extract_cfg<C, R, E>(
        self,
        e: E,
    ) -> Pipeline<R, CfgExtractor<E>, R::Driver, NopTransformation>
    where
        E: FnMut(&C) -> R,
        R: Fragment,
    {
        Pipeline {
            name: self.name,
            _fragment: PhantomData,
            extractor: CfgExtractor(e),
            driver: Default::default(),
            transformation: NopTransformation,
        }
    }
}

impl<F, E, D, T> Pipeline<F, E, D, T>
where
    F: Fragment,
{
    pub fn set_driver<ND: Driver<F>>(self, driver: ND) -> Pipeline<F, E, ND, T>
    where
        T: Transformation<<ND::SubFragment as Fragment>::Resource, F::Installer, ND::SubFragment>,
    {
        Pipeline {
            driver,
            name: self.name,
            _fragment: PhantomData,
            extractor: self.extractor,
            transformation: self.transformation,
        }
    }
}

impl<F, E, D, T> Pipeline<F, E, D, T>
where
    F: Fragment,
    D: Driver<F>,
    T: Transformation<<D::SubFragment as Fragment>::Resource, F::Installer, D::SubFragment>,
{
    pub fn transform<NT>(self, transform: NT) -> Pipeline<F, E, D, ChainedTransformation<T, NT>>
    where
        NT: Transformation<T::OutputResource, T::OutputInstaller, D::SubFragment>,
    {
        Pipeline {
            name: self.name,
            _fragment: PhantomData,
            driver: self.driver,
            extractor: self.extractor,
            transformation: ChainedTransformation(self.transformation, transform),
        }
    }

    pub fn set_installer<I, Opts, Config>(
        self,
        installer: I,
    ) -> Pipeline<F, E, D, SetInstaller<T, I>>
    where
        I: Installer<T::OutputResource, Opts, Config>,
    {
        Pipeline {
            name: self.name,
            _fragment: PhantomData,
            driver: self.driver,
            extractor: self.extractor,
            transformation: SetInstaller(self.transformation, Some(installer)),
        }
    }

    // TODO: add_installer
}

impl<B, E, D, T> Extension<B> for Pipeline<E::Fragment, E, D, T>
where
    B: Extensible<Ok = B>,
    B::Opts: Send + 'static,
    B::Config: Send + 'static,
    D: Driver<E::Fragment> + Send + 'static,
    E: Extractor<B::Opts, B::Config> + Send + 'static,
    T: Transformation<
        <D::SubFragment as Fragment>::Resource,
        <D::SubFragment as Fragment>::Installer,
        D::SubFragment,
    >,
    T: Send + 'static,
    T::OutputInstaller: Installer<T::OutputResource, B::Opts, B::Config>,
    T::OutputResource: Send + 'static,
    T::OutputInstaller: Send + 'static,
{
    // TODO: Extract parts & make it possible to run independently?
    // TODO: There seems to be a lot of mutexes that are not really necessary here.
    fn apply(self, builder: B) -> Result<B, Error> {
        let name = self.name;
        let mut transformation = self.transformation;
        let mut installer = transformation.installer(Default::default(), self.name);
        let builder = installer.init(builder)?;
        let install_cache = Arc::new(Mutex::new(InstallCache::new(installer)));
        let driver = Arc::new(Mutex::new(self.driver));
        let mut extractor = self.extractor;
        let validator = move |_old: &_, cfg: &mut B::Config, opts: &B::Opts| -> ValidationResults {
            let fragment = extractor.extract(opts, cfg);
            let instructions =
                match driver
                    .lock()
                    .instructions(&fragment, &mut transformation, name)
                {
                    Ok(i) => i,
                    Err(errs) => return errs.into(),
                };
            let driver_f = Arc::clone(&driver);
            let failure = move || {
                driver_f.lock().abort();
            };
            let driver_s = Arc::clone(&driver);
            let install_cache = Arc::clone(&install_cache);
            let success = move || {
                driver_s.lock().confirm();
                let mut install_cache = install_cache.lock();
                for ins in instructions {
                    install_cache.interpret(ins);
                }
            };
            ValidationResult::nothing()
                .on_abort(failure)
                .on_success(success)
                .into()
        };
        builder.config_validator(validator)
    }
}