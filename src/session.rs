use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;
use async_trait::async_trait;
use futures::{stream, Stream, StreamExt};
use log::{info, trace, warn};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use dashmap::DashMap;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;
use tokio::sync::{Mutex};
use std::sync::RwLock as StdRwLock;

use crate::peripheral::{self, Peripheral};
use crate::{MacAddressType, PeripheralPropertyId, PlatformPeripheralHandle};
use crate::{fake, winrt, Address, AddressType, MAC, Error, PlatformPeripheralProperty, Result};
use crate::{Event, PlatformEvent};

use anyhow::anyhow;

#[derive(Clone, Debug)]
pub struct Session {
    inner: Arc<SessionInner>,
}

#[derive(Debug)]
struct SessionInner {

    // The public-facing event stream
    event_bus: broadcast::Sender<Event>,

    // The private stream of events from the backend
    platform: PlatformSessionImpl,

    // Note: we have a (tokio) mutex here to synchronize while starting/stopping
    // scanning, not just for maintaining this is_scanning itself, so this can't
    // just be an AtomicBool
    is_scanning: Mutex<bool>,

    // All the state tracking for peripherals
    peripherals: DashMap<PlatformPeripheralHandle, PeripheralState>
}

#[async_trait]
pub(crate) trait PlatformSession {
    async fn start_scanning(&self, filter: &Filter) -> Result<()>;
    async fn stop_scanning(&self) -> Result<()>;

    async fn connect_peripheral(&self, peripheral_handle: PlatformPeripheralHandle) -> Result<()>;
}

#[derive(Debug)]
enum PlatformSessionImpl {
    #[cfg(target_os = "windows")]
    Winrt(winrt::session::WinrtSession),
    Fake(fake::session::FakeSession),
}
impl PlatformSessionImpl {
    fn api(&self) -> &dyn PlatformSession {
        match self {
            PlatformSessionImpl::Winrt(winrt) => winrt,
            PlatformSessionImpl::Fake(fake) => fake,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PeripheralState
{
    // Note we use a std::sync RwLock instead of a tokio RwLock while
    // we don't really expect any significant contention and so we can
    // support a simpler, synchronous api for reading peripheral
    // properties instead of needing to await for every property read
    pub(crate) inner: Arc<StdRwLock<PeripheralStateInner>>
}

#[derive(Debug, Default)]
pub(crate) struct PeripheralStateInner {
    // We wait until we have an address and name before we advertise
    // a Peripheral to applications.
    advertised: bool,

    pub(crate) address: Option<Address>,
    pub(crate) name: Option<String>,
    pub(crate) address_type: Option<MacAddressType>,
    pub(crate) tx_power: Option<i16>,
    pub(crate) rssi: Option<i16>,
    pub(crate) manufacturer_data: HashMap<u16, Vec<u8>>,
    pub(crate) service_data: HashMap<Uuid, Vec<u8>>,
    pub(crate) services: HashSet<Uuid>,
    pub(crate) is_connected: bool,
}

impl PeripheralState {
    fn new() -> Self {
        PeripheralState {
            inner: Arc::new(StdRwLock::new(PeripheralStateInner::default()))
        }
    }
}

pub struct Filter {

}
impl Filter {
    pub fn new() -> Self {
        Filter {}
    }
    pub fn by_address(&mut self, address: Address) -> &mut Self {
        todo!();
        self
    }
    pub fn by_services(&mut self, uuids: HashSet<Uuid>) -> &mut Self {
        todo!();
        self
    }
}

/*
Note: the initial scanning API supported ref-counting subscriptions to
scan and returned a ScanSubscription that would drop the ref count when
it got dropped. That sounded like a neat idea but ended up feeling
kinda awkward to use. Maybe I'll revisit this later though.

It's a bit fiddly that different platforms may handle attempts
to start multiple scans differently and potentially it'd be good to
enforce consistent behaviour by ensuring we only ever ask the
backend to scan with one filter which would be the union of filters
if the app wants to create multiple scanners.

From an application POV though I'm not sure there's really much
need to have multiple scans, and it could just be fine to enforce
at this level that it's an error to try and start multiple scans.

pub struct ScanSubscription {
    session: Session,
}
impl Drop for ScanSubscription {
    fn drop(&mut self) {
    }
}
*/

pub enum Backend {
    SystemDefault,
    Fake,
}
pub struct SessionConfig {
    backend: Backend,
}

impl SessionConfig {
    pub fn new() -> SessionConfig {
        SessionConfig {
            backend: Backend::SystemDefault,
        }
    }

    pub fn set_backend(&mut self, backend: Backend) -> &mut Self {
        self.backend = backend;
        self
    }

    pub async fn start(self) -> Result<Session> {
        Session::start(self).await
    }
}

impl Session {

    async fn start(config: SessionConfig) -> Result<Self> {
        let (broadcast_sender, _) = broadcast::channel(16);

        // Each per-platform backend is responsible for feeding the platform event bus
        // and then we handle state tracking and forwarding corresponding events to
        // the application as necessary
        let (platform_bus_tx, platform_bus_rx) = mpsc::unbounded_channel();
        let platform = match config.backend {
            #[cfg(target_os = "windows")]
            Backend::SystemDefault => {
                let implementation =
                    winrt::session::WinrtSession::new(&config, platform_bus_tx.clone()).await?;
                PlatformSessionImpl::Winrt(implementation)
            }
            Backend::Fake => {
                let implementation =
                    fake::session::FakeSession::new(&config, platform_bus_tx.clone()).await?;
                PlatformSessionImpl::Fake(implementation)
            }
        };
        let session = Session {
            inner: Arc::new(SessionInner {
                event_bus: broadcast_sender,
                //platform_bus: platform_bus_rx,
                platform,
                is_scanning: Mutex::new(false),
                //scan_subscriptions: AtomicU32::new(0),
                peripherals: DashMap::new()
            }),
        };

        let session_clone = session.clone();
        tokio::spawn(async move { session_clone.run(platform_bus_rx).await });

        Ok(session)
    }

    pub(crate) fn get_peripheral_state(&self, peripheral_handle: PlatformPeripheralHandle) -> PeripheralState {
        match self.inner.peripherals.get(&peripheral_handle) {
            Some(peripheral_state) => peripheral_state.clone(),
            None => {
                let peripheral_state = PeripheralState::new();
                self.inner.peripherals.insert(peripheral_handle, peripheral_state.clone());
                peripheral_state
            }
        }
    }

    async fn run(self, platform_bus: mpsc::UnboundedReceiver<PlatformEvent>) {
        trace!("Processing platform bus...");

        let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(platform_bus);
        tokio::pin!(stream);
        while let Some(event) = stream.next().await {
            match event {
                PlatformEvent::PeripheralFound { peripheral_handle } => {
                    // XXX: we actually defer notifying the app until we at least know the name + address
                    // TODO: can probably remove this PlatformEvent - or maybe allocate PeripheralState here
                    // and then add an error check later that the state is expected to exist.
                }
                PlatformEvent::PeripheralConnected { peripheral_handle } => {
                    let peripheral_state = self.get_peripheral_state(peripheral_handle);
                    let mut state_guard = peripheral_state.inner.write().unwrap();

                    if state_guard.is_connected != true {
                        trace!("PeripheralConnected: handle={}/{}",
                                peripheral_handle.0,
                                state_guard.address.as_ref().unwrap_or(&Address::MAC(MAC(0))).to_string());
                        state_guard.is_connected = true;

                        trace!("Notifying peripheral {} connected", state_guard.address.as_ref().unwrap().to_string());
                        let _ = self.inner.event_bus.send(Event::PeripheralConnected(Peripheral::wrap(self.clone(), peripheral_handle)));
                    } else {
                        warn!("Spurious, unbalanced/redundant PeripheralConnected notification from backend");
                    }
                }
                PlatformEvent::PeripheralDisconnected { peripheral_handle } => {
                    let peripheral_state = self.get_peripheral_state(peripheral_handle);
                    let mut state_guard = peripheral_state.inner.write().unwrap();

                    if state_guard.is_connected != false {
                        trace!("PeripheralDisconnected: handle={}/{}",
                                peripheral_handle.0,
                                state_guard.address.as_ref().unwrap_or(&Address::MAC(MAC(0))).to_string());
                        state_guard.is_connected = false;

                        trace!("Notifying peripheral {} disconnected", state_guard.address.as_ref().unwrap().to_string());
                        let _ = self.inner.event_bus.send(Event::PeripheralDisconnected(Peripheral::wrap(self.clone(), peripheral_handle)));
                    } else {
                        warn!("Spurious, unbalanced/redundant PeripheralDisonnected notification from backend");
                    }
                }
                PlatformEvent::PeripheralPropertySet { peripheral_handle, property } => {
                    let peripheral_state = self.get_peripheral_state(peripheral_handle);

                    //
                    // XXX: BEWARE: This is a std::sync lock so we need to avoid awaiting or taking too long
                    // while we currently have a broad scope for convenience here...
                    //
                    let mut state_guard = peripheral_state.inner.write().unwrap();

                    let mut changed_prop = None;
                    //let mut changed_connection_state = false;


                    trace!("PeripheralPropertySet: handle={}/{}: {:?}",
                            peripheral_handle.0,
                            state_guard.address.as_ref().unwrap_or(&Address::MAC(MAC(0))).to_string(),
                            property);

                    match property {
                        PlatformPeripheralProperty::Address(address) => {
                            // XXX: in terms of the public API the address isn't expected to change for a
                            // specific Peripheral so this won't be reported as a PropertyChange
                            // Once we have an address and name though we will advertise the Peripheral
                            // to the application.
                            match &state_guard.address {
                                None => {
                                    state_guard.address = Some(address);
                                }
                                Some(existing) => {
                                    if &address != existing {
                                        log::error!("Spurious change of peripheral address by backend!");
                                    }
                                }
                            }
                        }
                        PlatformPeripheralProperty::AddressType(adddress_type) => {
                            match adddress_type {
                                AddressType::PublicMAC => {
                                    state_guard.address_type = Some(MacAddressType::Public);
                                }
                                AddressType::RandomMAC => {
                                    state_guard.address_type = Some(MacAddressType::Random);
                                }
                                AddressType::String => {
                                    state_guard.address_type = None;
                                }
                            }
                            changed_prop = Some(PeripheralPropertyId::AddressType);
                        }
                        PlatformPeripheralProperty::Name(name) => {
                            let changed = match &state_guard.name {
                                Some(current_name) => current_name != &name,
                                None => true,
                            };
                            if changed {
                                state_guard.name = Some(name);
                                changed_prop = Some(PeripheralPropertyId::Name);
                            }
                        }
                        PlatformPeripheralProperty::TxPower(tx_power) => {
                            if state_guard.tx_power.is_none() || state_guard.tx_power.unwrap() != tx_power {
                                state_guard.tx_power = Some(tx_power);
                                changed_prop = Some(PeripheralPropertyId::TxPower);
                            }
                        }
                        PlatformPeripheralProperty::Rssi(rssi) => {
                            if state_guard.rssi.is_none() || state_guard.rssi.unwrap() != rssi {
                                state_guard.rssi = Some(rssi);
                                changed_prop = Some(PeripheralPropertyId::Rssi);
                            }
                        }
                        PlatformPeripheralProperty::ManufacturerData(data) => {
                            if !data.is_empty() {
                                // Assume something may have changed without doing a detailed
                                // comparison of state...
                                state_guard.manufacturer_data.extend(data);
                                changed_prop = Some(PeripheralPropertyId::ManufacturerData);
                            }
                        }
                        PlatformPeripheralProperty::ServiceData(data) => {
                            if !data.is_empty() {
                                // Assume something may have changed without doing a detailed
                                // comparison of state...
                                state_guard.service_data.extend(data);
                                changed_prop = Some(PeripheralPropertyId::ServiceData);
                            }
                        }
                        PlatformPeripheralProperty::Services(uuids) => {
                            let mut new_services = false;
                            for uuid in uuids {
                                if !state_guard.services.contains(&uuid) {
                                    state_guard.services.insert(uuid);
                                    new_services = true;
                                }
                            }
                            if new_services {
                                changed_prop = Some(PeripheralPropertyId::Services);
                            }
                        }
                        /*
                        PlatformPeripheralProperty::Connected(is_connected) => {
                            if state_guard.is_connected != is_connected {
                                state_guard.is_connected = is_connected;
                                changed_connection_state = true;
                            }
                        }
                        */
                        //PlatformPeripheralProperty::Paired(is_paired) => {
                        //    todo!()
                        //}
                    }

                    // We wait until we have and address and a name before advertising peripherals
                    // to applications...
                    if state_guard.advertised == false && state_guard.name != None && state_guard.address != None {
                        let _ = self.inner.event_bus.send(Event::PeripheralFound {
                            peripheral: Peripheral::wrap(self.clone(), peripheral_handle),
                            address: state_guard.address.as_ref().unwrap().to_owned(),
                            name: state_guard.name.as_ref().unwrap().clone()
                        });
                        state_guard.advertised = true;

                        // Also notify the app about any other properties we we're already tracking for the peripheral...

                        //let _ = self.inner.event_bus.send(Event::DevicePropertyChanged(Peripheral::wrap(self.clone(), peripheral_handle), PeripheralPropertyId::Address));
                        let _ = self.inner.event_bus.send(Event::PeripheralPropertyChanged(Peripheral::wrap(self.clone(), peripheral_handle), PeripheralPropertyId::Name));
                        if state_guard.address_type.is_some() {
                            let _ = self.inner.event_bus.send(Event::PeripheralPropertyChanged(Peripheral::wrap(self.clone(), peripheral_handle), PeripheralPropertyId::AddressType));
                        }
                        if state_guard.tx_power.is_some() {
                            let _ = self.inner.event_bus.send(Event::PeripheralPropertyChanged(Peripheral::wrap(self.clone(), peripheral_handle), PeripheralPropertyId::TxPower));
                        }
                        if state_guard.rssi.is_some() {
                            let _ = self.inner.event_bus.send(Event::PeripheralPropertyChanged(Peripheral::wrap(self.clone(), peripheral_handle), PeripheralPropertyId::Rssi));
                        }
                        if !state_guard.manufacturer_data.is_empty() {
                            let _ = self.inner.event_bus.send(Event::PeripheralPropertyChanged(Peripheral::wrap(self.clone(), peripheral_handle), PeripheralPropertyId::ManufacturerData));
                        }
                        if !state_guard.service_data.is_empty() {
                            let _ = self.inner.event_bus.send(Event::PeripheralPropertyChanged(Peripheral::wrap(self.clone(), peripheral_handle), PeripheralPropertyId::ServiceData));
                        }
                        if !state_guard.services.is_empty() {
                            let _ = self.inner.event_bus.send(Event::PeripheralPropertyChanged(Peripheral::wrap(self.clone(), peripheral_handle), PeripheralPropertyId::Services));
                        }
                    }
                    /*
                    if changed_connection_state {
                        if state_guard.is_connected {
                            trace!("Notifying peripheral {} connected", state_guard.address.as_ref().unwrap().to_string());
                            let _ = self.inner.event_bus.send(Event::PeripheralConnected(Peripheral::wrap(self.clone(), peripheral_handle)));
                        } else {
                            trace!("Notifying peripheral {} disconnected", state_guard.address.as_ref().unwrap().to_string());
                            let _ = self.inner.event_bus.send(Event::PeripheralDisconnected(Peripheral::wrap(self.clone(), peripheral_handle)));
                        }
                    }
                    */
                    if let Some(changed_prop) = changed_prop {
                        trace!("Notifying property {:?} changed", changed_prop);
                        let _ = self.inner.event_bus.send(Event::PeripheralPropertyChanged(Peripheral::wrap(self.clone(), peripheral_handle), changed_prop));
                    }
                }
            }
        }
    }

    pub fn events(&self) -> Result<impl Stream<Item = Event>> {
        let receiver = self.inner.event_bus.subscribe();
        Ok(BroadcastStream::new(receiver).filter_map(|x| async move {
            if x.is_ok() {
                Some(x.unwrap())
            } else {
                None
            }
        }))
    }

    /// Starts scanning for Bluetooth devices, according to the given filter
    ///
    /// Note: It's an error to try and initiate multiple scans in parallel
    /// considering the varied ways different platforms will try to handle
    /// such requests.
    pub async fn start_scanning(&self, filter: Filter) -> Result<()> {
        let mut is_scanning_guard = self.inner.is_scanning.lock().await;

        if *is_scanning_guard {
            return Err(Error::Other(anyhow!("Already scanning")));
        }

        self.inner.platform.api().start_scanning(&filter).await?;
        *is_scanning_guard = true;

        Ok(())
    }

    pub async fn stop_scanning(&self) -> Result<()> {
        let mut is_scanning_guard = self.inner.is_scanning.lock().await;
        if !*is_scanning_guard {
            return Err(Error::Other(anyhow!("Not currently scanning")));
        }

        self.inner.platform.api().stop_scanning().await?;
        *is_scanning_guard = false;

        Ok(())
    }

    pub(crate) async fn connect_peripheral(&self, peripheral_handle: PlatformPeripheralHandle) -> Result<()> {
        self.inner.platform.api().connect_peripheral(peripheral_handle).await
    }
}
