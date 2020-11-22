use anyhow::Error;
use yew::prelude::*;
use yew::services::{ConsoleService,Task};
use yew::{html, ComponentLink, Html};
use yew_router::{route::Route, service::RouteService};
use yew_router::{Switch};
use yew::format::{Json,Nothing};
use yew::services::fetch::{FetchTask};
use yew::services::interval::{IntervalService};
use yew::services::reader::{File, FileData, ReaderService, ReaderTask};
use yew::services::websocket::{WebSocketStatus};
use yew::services::fetch::{FetchService, Request, Response};
use web_sys::{WebSocket,BinaryType,MessageEvent};
use uuid::Uuid;
use std::str;
use web_sys::{AudioContext, AudioBuffer, AudioBufferSourceNode};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{spawn_local, JsFuture};

use ham_rs::Call;
use ham_rs::countries::{CountryInfo,Country};
use ham_rs::rig::{Receiver,Radio,Version,Command,CommandResponse,RECEIVER_MODES,Mode,Spot};
use ham_rs::log::LogEntry;

pub struct Model {
    // Currently unused
    pub route_service: RouteService<()>,
    pub route: Route<()>,
    // Console logging
    pub console: ConsoleService,
    // Callbacks
    pub link: ComponentLink<Self>,

    // SparkSDR connection
    wss: Option<WebSocket>,
    // List of receivers from getReceivers command
    receivers: Vec<Receiver>,
    // List of radios from the getRadios command
    radios: Vec<Radio>,
    // Currently selected receiver
    default_receiver: Option<Uuid>,
    // Version response from the getVersion command
    version: Option<Version>,
    // TODO: temporary to keep local ui updated
    poll: Option<Box<dyn Task>>,
    // Spots from enabling SubscribeToSpots
    spots: Vec<Spot>,
    // Show/Hide receiver list
    show_receiver_list: bool,
    // Imported log file (ADIF format) for spot cross checking
    import: Option<Vec<LogEntry>>,
    // Services for file importing (log file)
    reader: ReaderService,
    tasks: Vec<ReaderTask>,
    // Local callsign cache
    callsigns: Vec<CallsignInfo>,
    // audio playback
    audio_ctx: AudioContext,
    source: AudioBufferSourceNode,
    buffer: Rc<RefCell<Option<AudioBuffer>>>,
}

// Currently this is unused as there is only one route: /
#[derive(Clone,Switch, Debug)]
pub enum AppRoute {
    #[to = "/"]
    Index,
}

// Currently only TextMsg is implemented and this is the
// commands to and responses from SparkSDR.
// BinaryMsg would be for future support for audio in/out
pub enum WebsocketMsgType {
    BinaryMsg(js_sys::ArrayBuffer),
    TextMsg(String)
}

type Chunks = bool;

// Used with the local callsign cache for our requests
// for callsign info.
pub enum CallsignInfo {
    Requested((Call, FetchTask)),
    Found(Call),
    NotFound(Call)
}

impl CallsignInfo {
    pub fn call(&self) -> Call {
        match self {
            CallsignInfo::Requested((c, _)) => c.clone(),
            CallsignInfo::Found(c) => c.clone(),
            CallsignInfo::NotFound(c) => c.clone(),
        }
    }
}

pub enum Msg {
    // not implemented
    RouteChanged(Route<()>),
    ChangeRoute(AppRoute),
    // websocket connection
    Disconnected,
    Connected,
    // Command responses from SparkSDR (e.g. getReceiversResponse, getVersionResponse)
    CommandResponse(Result<CommandResponse, Error>),
    // UI request frequency change up/down on digit X for receiver Uuid
    FrequencyUp(Uuid, i32), // digit 0 - 8 
    FrequencyDown(Uuid, i32), // digit 0 - 8
    // UI request to change receiver Uuid mode
    ModeChanged(Uuid, Mode),
    // UI request to set the default receiver to Uuid
    SetDefaultReceiver(Uuid),
    // UI request to add a receiver to radio Uuid
    AddReceiver(Uuid),
    // UI request to remove a receiver by Uuid
    RemoveReceiver(Uuid),
    // Toggle radio power state
    TogglePower(Uuid),
    // Not implemented (future support for audio data)
    ReceivedAudio(js_sys::ArrayBuffer),
    // UI toggle show/hide receiver list
    ToggleReceiverList,
    // None
    None,
    // TODO: poll tick (temporary)
    Tick,
    // File import (log file)
    Files(Vec<File>, Chunks),
    Loaded(FileData),
    CancelImport,
    ConfirmImport,
    // Response to our callsign info request
    CallsignInfoReady(Result<Call,Error>)
}

impl Model {
    pub fn new(link: ComponentLink<Self>) -> Model {
        let mut route_service: RouteService<()> = RouteService::new();
        let route = route_service.get_route();
        let callback = link.callback(Msg::RouteChanged);
        route_service.register_callback(callback);

        // audio channel
        let buffer = Rc::new(RefCell::new(None));
        let audio_ctx = web_sys::AudioContext::new().unwrap();
        let source = audio_ctx.create_buffer_source().unwrap();

        let destination = audio_ctx.destination();
        let gain = audio_ctx.create_gain().unwrap();
        gain.gain().set_value(1.0);
        gain.connect_with_audio_node(&destination).unwrap();
        source.connect_with_audio_node(&gain).unwrap();

        let analyzer = audio_ctx.create_analyser().unwrap();
        analyzer.connect_with_audio_node(&destination).unwrap();

        source.set_loop(false);
        source.start().unwrap();

        Model {
            route_service,
            route,
            link,
            console: ConsoleService::new(),
            wss: None,
            receivers: vec![],
            radios: vec![],
            default_receiver: None,
            version: None,
            poll: None,
            spots: vec![],
            show_receiver_list: false,
            import: None,
            reader: ReaderService::new(),
            tasks: Vec::new(),
            callsigns: vec![],
            audio_ctx: audio_ctx,
            source: source,
            buffer: buffer,
        }
    }

    pub fn set_receivers(&mut self, receivers: Vec<Receiver>) {
        self.receivers = receivers;
    }

    pub fn set_radios(&mut self, radios: Vec<Radio>) {
        self.radios = radios;
    }

    pub fn set_version(&mut self, version: Version) {
        self.version = Some(version);
    }

    pub fn set_default_receiver(&mut self, receiver: Option<Uuid>) {
        self.default_receiver = receiver;
    }

    pub fn toggle_receiver_list(&mut self) {
        self.show_receiver_list = !self.show_receiver_list;
    }

    pub fn change_receiver_mode(&mut self, receiver_id: Uuid, mode: Mode) {
        if let Some(index) = self.receivers.iter().position(|i| i.id == receiver_id) {
            self.receivers[index].mode = mode.clone();
            self.send_command(Command::SetMode { Mode: mode.clone(), ID: receiver_id });
        }
    }

    pub fn frequency_up(&mut self, receiver_id: Uuid, digit: i32) {
        if let Some(index) = self.receivers.iter().position(|i| i.id == receiver_id) {
            if digit == 0 { self.receivers[index].frequency += 100000000.0 }
            if digit == 1 { self.receivers[index].frequency += 10000000.0 }
            if digit == 2 { self.receivers[index].frequency += 1000000.0 }
            if digit == 3 { self.receivers[index].frequency += 100000.0 }
            if digit == 4 { self.receivers[index].frequency += 10000.0 }
            if digit == 5 { self.receivers[index].frequency += 1000.0 }
            if digit == 6 { self.receivers[index].frequency += 100.0 }
            if digit == 7 { self.receivers[index].frequency += 10.0 }
            if digit == 8 { self.receivers[index].frequency += 1.0 }

            self.send_command(Command::SetFrequency { Frequency: (self.receivers[index].frequency as i32).to_string(), ID: receiver_id });
        }
    }

    pub fn frequency_down(&mut self, receiver_id: Uuid, digit: i32) {
        if let Some(index) = self.receivers.iter().position(|i| i.id == receiver_id) {
            if digit == 0 { self.receivers[index].frequency -= 100000000.0 }
            if digit == 1 { self.receivers[index].frequency -= 10000000.0 }
            if digit == 2 { self.receivers[index].frequency -= 1000000.0 }
            if digit == 3 { self.receivers[index].frequency -= 100000.0 }
            if digit == 4 { self.receivers[index].frequency -= 10000.0 }
            if digit == 5 { self.receivers[index].frequency -= 1000.0 }
            if digit == 6 { self.receivers[index].frequency -= 100.0 }
            if digit == 7 { self.receivers[index].frequency -= 10.0 }
            if digit == 8 { self.receivers[index].frequency -= 1.0 }

            self.send_command(Command::SetFrequency { Frequency: (self.receivers[index].frequency as i32).to_string(), ID: receiver_id });
        }
    }

    pub fn cache_callsign_info(&mut self, call: Call) {
        let indexes : Vec<usize> = self.spots.iter().enumerate().filter(|&(_, s)| s.call.call() == call.call() ).map(|(i, _)| i).collect();
        for index in indexes {
            // update spot record with our updated callsign info
            self.spots[index].call = call.clone();
        }

        // Mark callsign as found in local callsign cache for
        // future lookups
        if let Some(index) = self.callsigns.iter().position(|c| c.call().call() == call.call()) {
            self.callsigns[index] = CallsignInfo::Found(call)
        } else {
            self.callsigns.push(CallsignInfo::Found(call))
        }
    }

    // Two channels for the websocket connection
    // 1) Text: Json encoded messages for control/info (e.g. get/set frequency)
    // 2) Binary: Binary encoded audio data
    //
    // Both channels are bi-directional (e.g. transmit using binary encoded audio)
    // 
    pub fn connect(&mut self, ws: &str) {
        let ws = WebSocket::new(ws).unwrap();
        ws.set_binary_type(BinaryType::Arraybuffer);

		let cbnot = self.link.callback(|input| {
			match input {
				WebSocketStatus::Closed | WebSocketStatus::Error => {
					Msg::Disconnected
                },
                WebSocketStatus::Opened => {
                    Msg::Connected
                }
			}
        });
        
        let notify = cbnot.clone();
        let onopen_callback = Closure::wrap(Box::new(move |_| {
            ConsoleService::new().log("rig control: connection opened");
            notify.emit(WebSocketStatus::Opened);
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onopen(Some(onopen_callback.as_ref().unchecked_ref()));
        onopen_callback.forget();

        let notify = cbnot.clone();
        let onerror_callback = Closure::wrap(Box::new(move |_| {
            ConsoleService::new().log("rig control: connection error");
            notify.emit(WebSocketStatus::Error);
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onerror(Some(onerror_callback.as_ref().unchecked_ref()));
        onerror_callback.forget();

        let notify = cbnot.clone();
        let onclose_callback = Closure::wrap(Box::new(move |_| {
            ConsoleService::new().log("rig control: connection closed");
            notify.emit(WebSocketStatus::Closed);
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onclose(Some(onclose_callback.as_ref().unchecked_ref()));
        onclose_callback.forget();

        let cbout = self.link.callback(|data| {
            match data {
                WebsocketMsgType::BinaryMsg(binary) => {
                    Msg::ReceivedAudio(binary)
                },
                WebsocketMsgType::TextMsg(text) => {
                    let Json(data): Json<Result<CommandResponse, _>> = Json::from(Ok(text));
                    Msg::CommandResponse(data)
                }
            }
        });
        let onmessage_callback = Closure::wrap(Box::new(move |e: MessageEvent| {
            if let Ok(abuf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                //let array = js_sys::Uint8Array::new(&abuf);
                cbout.emit(WebsocketMsgType::BinaryMsg(abuf));
            } else if let Ok(_blob) = e.data().dyn_into::<web_sys::Blob>() {
                ConsoleService::new().log("rig control: unexpected blob message from server");
            } else if let Ok(txt) = e.data().dyn_into::<js_sys::JsString>() {
                cbout.emit(WebsocketMsgType::TextMsg(txt.into()));
            } else {
                ConsoleService::new().log("rig control: unexpected message from server");
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(onmessage_callback.as_ref().unchecked_ref()));
        onmessage_callback.forget();
        self.wss = Some(ws)
    }

    pub fn disconnect(&mut self) {
        self.wss = None;
    }

    pub fn is_connected(&self) -> bool {
        match self.wss {
            Some(_) => true,
            None => false
        }
    }

    pub fn send_command(&mut self, cmd: Command) {
        let j = serde_json::to_string(&cmd).unwrap();
        if let Some(wss) = &self.wss {
            wss.send_with_str(&j).unwrap();
            self.console.log(&format!("sent: {}", j));
        } else {
            self.console.error(&format!("attempted to send: {}, but not connected", j));
        }
    }

    pub fn handle_audio_data(&mut self, data: js_sys::ArrayBuffer) {
        // Both the commented out code and what is below are broken in different ways
        // The uncommented code will play the incoming data immedietly on top of what
        // is currently playing, The commented out code will _replace_ what is currently
        // playing with the incoming data.
        //
        // TODO: need to sequence the incoming data to play immedietly after previous
        // data finishes playing.
        //

        //let moved_buffer = self.buffer.clone();
        //let moved_source = self.source.clone();

        let moved_context = self.audio_ctx.clone();
        
        spawn_local(async move {
            Some(async move {
                let buffer = JsFuture::from(moved_context.decode_audio_data(&data)?)
                        .await?
                        .dyn_into::<AudioBuffer>();

                let moved_buffer = buffer.clone();
                match moved_buffer {
                    Ok(moved_buffer) => {
                        let source = moved_context.create_buffer_source().unwrap();
                        source.set_buffer(Some(&moved_buffer));
                        let destination = moved_context.destination();
                        source.connect_with_audio_node(&destination).unwrap();
                        source.set_loop(false);
                        source.start().unwrap();
                    },
                    Err(err) => {
                        ConsoleService::new().log(&format!("audo buffer error: {:?}", err));
                    }
                }
                buffer
            }.await.unwrap());
        });

        /*spawn_local(async move {
            *moved_buffer.borrow_mut() = Some(async move {
                //JsFuture::from(decode_audio(&data))
                //    .await?
                //    .dyn_into::<AudioBuffer>()
                let buffer = JsFuture::from(moved_context.decode_audio_data(&data)?)
                    .await?
                    .dyn_into::<AudioBuffer>();

                let moved_buffer = buffer.clone();
                match moved_buffer {
                    Ok(moved_buffer) => {
                        // TODO: need some kind of buffer here to append the new
                        // audio data to instead of replacing it
                        ConsoleService::new().log("decoded audio. adding to buffer.");
                        //let source = moved_context.create_buffer_source().unwrap();
                        moved_source.set_buffer(Some(&moved_buffer));

                        /*let destination = moved_context.destination();
                        let gain = moved_context.create_gain().unwrap();
                        gain.gain().set_value(1.0);
                        gain.connect_with_audio_node(&destination).unwrap();
                        source.connect_with_audio_node(&gain).unwrap();

                        source.set_loop(false);
                        source.start().unwrap();*/
                    },
                    Err(err) => {
                        ConsoleService::new().log(&format!("audo buffer error: {:?}", err));
                    }
                }

                buffer
            }.await.unwrap());
        });*/
    }

    pub fn enable_ticks(&mut self, interval: u64) {
        let mut is = IntervalService::new();
        let handle = is.spawn(
            Duration::from_secs(interval),
            self.link.callback(|_| Msg::Tick),
        );
        self.poll = Some(Box::new(handle))
    }

    pub fn trim_spots(&mut self, limit: usize) {
        if self.spots.len() > limit {
            let drain = self.spots.len() - limit;
            self.spots.drain(0..drain);
        }
    }

    pub fn add_spot(&mut self, spot: Spot) {
        // FIXME: temp fix
        let mut spot = spot;

        if let Some(index) = self.callsigns.iter().position(|c| c.call().call() == spot.call.call() ) {
            match &self.callsigns[index] {
                CallsignInfo::Found(call) => {
                    // update spot call with additional callsign info from cache
                    let call = call.clone();
                    spot.call = call;
                },
                _ => ()
            }
        } else {
            let call = spot.call.clone();
            let callback = self.link.callback(
                move |response: Response<Json<Result<Call, Error>>>| {
                    let (meta, Json(data)) = response.into_parts();
                    if meta.status.is_success() {
                        Msg::CallsignInfoReady(data)
                    } else {
                        Msg::None // FIXME: Handle this error accordingly.
                    }
                },
            );

            match call.prefix() {
                // If callsign is United States make a request for additional callsign
                // info from server.  Response will be handled by the Msg::CallsignInfoReady
                // message handler
                Some(prefix) if call.country() == Ok(Country::UnitedStates) => {
                    let mut fs = FetchService::new();
                    let request = Request::get(format!("/out/{}/{}.json", prefix, spot.call.call())).body(Nothing).unwrap();
                    let ft = fs.fetch(request, callback).unwrap();

                    let info = CallsignInfo::Requested((call, ft));
                    self.callsigns.push(info);
                },
                _ => ()
            }
        }

        self.spots.push(spot);
    }

    pub fn read_file(&mut self, file: File) {
        let task = {
            let callback = self.link.callback(|data| Msg::Loaded(data));
            self.reader.read_file(file, callback).unwrap()
        };
        self.tasks.push(task);
    }

    pub fn load_adif_data(&mut self, data: FileData) {
        match adif::adif_parse("import", &mut data.content.as_slice()) {
            Ok(adif) => {
                let mut records = Vec::new();
                for record in adif.adif_records.as_slice() {
                    match LogEntry::from_adif_record(&record) {
                        Ok(entry) => {
                            records.push(entry);
                        },
                        Err(e) => {
                            self.console.log(&format!("failed to import record [{:?}]: {:?}", e, record));
                        }
                    }
                }
                self.import = Some(records);
            },
            Err(e) => {
                self.console.log(&format!("unable to load adif: {}", e));
            }
        }
    }

    pub fn clear_adif_data(&mut self) {
        self.import = None;
    }

    pub fn update_receiver(&mut self, receiver_id: Uuid, mode: Mode, frequency: f32) {
        if let Some(index) = self.receivers.iter().position(|i| i.id == receiver_id) {
            self.receivers[index].frequency = frequency;
            self.receivers[index].mode = mode;
        } else {
            self.console.log(&format!("Attempted to update a receiver that does not exist: {}", receiver_id));
        }
    }

    pub fn get_radio_power_state(&self, radio_id: Uuid) -> Option<bool> {
        if let Some(index) = self.radios.iter().position(|i| i.id == radio_id) {
            Some(self.radios[index].running)
        } else {
            None
        }
    }

    pub fn radio_list(&self) -> Html {
        html! {
            { for self.radios.iter().map(|r| {
                    self.radio(&r)
                  })
            }
        }
    }

    pub fn toggle_receivers(&self) -> Html {
        let cls = if self.show_receiver_list == true {
            "fa-chevron-up"
        } else {
            "fa-chevron-down"
        };
        html! {
            <button class="button is-text" onclick=self.link.callback(move |_| Msg::ToggleReceiverList)>
                <span>{ format!("{} Receivers", self.receivers.len()) }</span>
                <span class="icon is-small">
                    <i class=("fas", cls)></i>
                </span>
            </button>
        }
    }

    pub fn receiver_list(&self) -> Html {
        match self.show_receiver_list {
            true => {
                html! {
                    {
                        for self.receivers.iter().map(|r| {
                            self.receiver(&r)
                        })
                    }
                }
            },
            false => html! {}
        }
    }

    pub fn spots_view(&self) -> Html {
        html! {
            <>
                <div style="clear:both"></div>

                <div class="s">
                    <table class="table">
                        <tr>
                            <th>{ "UTC" }</th>
                            <th>{ "dB" }</th>
                            <th>{ "DT" }</th>
                            <th>{ "Freq" }</th>
                            <th>{ "Mode" }</th>
                            <th>{ "Dist" }</th>
                            <th>{ "Message" }</th>
                            <th></th>
                            <th></th>
                            <th></th>
                        </tr>
                        { for self.spots.iter().rev().map(|s| {
                            self.spot(&s)
                          })
                        }
                    </table>
                </div>

                { self.import_adif_form() }
            </>
        }
    }

    pub fn version_html(&self) -> Html {
        match &self.version {
            Some(version) => html! { <p class="version">{ format!("{} {} [Protocol Version: {}]", version.host, version.host_version, version.protocol_version) }</p> },
            None => html! {},
        }
    }

    fn import_adif_form(&self) -> Html {
        html! {
                <div class="import">
                    {
                        match &self.import {
                            None => html! {
                                <>
                    <p>{"Import log file (adif format) to cross check spots."}</p>
                    <input type="file" multiple=true onchange=self.link.callback(move |value| {
                            let mut result = Vec::new();
                            if let ChangeData::Files(files) = value {
                                let files = js_sys::try_iter(&files)
                                    .unwrap()
                                    .unwrap()
                                    .into_iter()
                                    .map(|v| File::from(v.unwrap()));
                                result.extend(files);
                            }
                            Msg::Files(result, false)
                        })/>
                                </>
                            },
                            Some(import) => html! {
                                <>
                                    <p>{ format!("Found {} records", import.len()) }</p>
                                    <p>
                                        <input type="button" class="button" value="Cancel Import" onclick=self.link.callback(|_| Msg::CancelImport) />
                                    </p>
                                </>
                            },
                        }
                    }
                </div>
        }
    }

    fn spot(&self, spot: &Spot) -> Html {
        let call = Call::new(spot.call.to_string());
        let (country_icon, state_class) =
            match call.country() {
                Ok(country) => {
                    let (new_country, new_state) =
                        match &self.import {
                            Some(import) => {
                                let new_country =
                                    if let Some(_index) = import.iter().position(|i| i.call.country() == Ok(country.clone())) {
                                        ""
                                    } else {
                                        "new-country"
                                    };
                                let new_state =
                                    match call.state() {
                                        Some(state) => {
                                            if let Some(_index) = import.iter().position(|i| i.call.state() == Some(state.to_string())) {
                                                ""
                                            } else {
                                                "new-state"
                                            }
                                        },
                                        _ => "",
                                    };
                                (new_country, new_state)
                            },
                            None => ("", ""),
                        };
                    (html! { <><i class=format!("flag-icon flag-icon-{}", country.code())></i> <span class=new_country>{ country.name() }</span></> }, new_state)
                },
                Err(_) => (html! {}, ""),
            };

        html! {
            <tr>
                <td>{ spot.time.format("%H%M%S") }</td>
                <td>{ spot.snr }</td>
                <td>{ spot.dt }</td>
                <td>{ format!("{} (+{})", spot.tuned_frequency, (spot.frequency - spot.tuned_frequency)) }</td>
                <th>{ spot.mode.mode() }</th>
                <td>{ match spot.distance {
                         Some(dist) => format!("{}", dist),
                         None => format!(""),
                      }
                    }</td>
                {
                    match spot.msg.contains("CQ") {
                        true => html! { <th>{ spot.msg.to_string() }</th> },
                        false => html! { <td>{ spot.msg.to_string() }</td> }
                    }
                }
                <td>{ country_icon }</td>
                <td>{ match spot.call.state() {
                          Some(state) => format!("{}", state),
                          None => format!("")
                      } }</td>
                <td class=state_class>{ match spot.call.op() {
                          Some(op) => format!("{}", op),
                          None => format!("")
                      } }</td>
            </tr>
        }
    }

    pub fn radio(&self, radio: &Radio) -> Html {
        let radio_id = radio.id;
        let power_class =
            match radio.running {
                true => "icon is-small has-text-success",
                false => "icon is-small",
            };
        html! {
            <div class="radio-control">
                <button class="button is-text" disabled=true>
                    { radio.name.to_string() }
                </button>
                <button class="button" title="Power" onclick=self.link.callback(move |_| Msg::TogglePower(radio_id))>
                    <span class=power_class>
                    <i class="fas fa-power-off fa-lg"></i>
                    </span>
                </button>
                <button class="button" onclick=self.link.callback(move |_| Msg::AddReceiver(radio_id) ) title="Add Receiver">
                    <span class="icon is-small">
                    <i class="fas fa-plus fa-lg"></i>
                    </span>
                </button>
            </div>
        }
    }

    pub fn receiver(&self, receiver: &Receiver) -> Html {
        let frequency_string = format!("{:0>9}", receiver.frequency.to_string());
        let tmp = self.decimal_mark(frequency_string);
        let mut inactive = true;
        let receiver_id = receiver.id;
        let class_name = 
            if Some(receiver.id) == self.default_receiver {
                "receiver-control selected"
            } else {
                "receiver-control"
            };

        html! {
            <form class=class_name onclick=self.link.callback(move |_| Msg::SetDefaultReceiver(receiver_id))>
                <div class="up-controls">
                    {
                        for (0..9).map(|digit| {
                            html! { <><a onclick=self.link.callback(move |_| Msg::FrequencyUp(receiver_id, digit))>{ "0" }</a>{ if digit == 2 || digit == 5 { "," } else { "" } }</> }
                        })
                    }
                </div>
                <div id="frequency" class="frequency">
                    {
                        for tmp.chars().map(|c| {
                            if ((c != '0' && c != ',' && inactive == true)) {
                                inactive = false;
                            }
                            match c {
                                ',' => html! { { "," } },
                                _ if inactive => html! { <span>{ c.to_string() }</span> },
                                _ => html! { <span class="active">{ c.to_string() }</span> }
                            }
                        })
                    }
                </div>
                <div class="down-controls">
                    {
                        for (0..9).map(|digit| {
                            html! { <><a onclick=self.link.callback(move |_| Msg::FrequencyDown(receiver_id, digit))>{ "0" }</a>{ if digit == 2 || digit == 5 { "," } else { "" } }</> }
                        })
                    }
                </div>
                <div class="mode control" style="margin-top:-0.5em;z-index:50">
                    <button style="float:right" class="button is-text" onclick=self.link.callback(move |_| Msg::RemoveReceiver(receiver_id))>
                        <span class="icon is-small">
                        <i class="far fa-trash-alt"></i>
                        </span>
                    </button>
                    <select id="mode" class="select" 
                        onchange=self.link.callback(move |e:ChangeData| 
                            match e {
                                ChangeData::Select(sel) => {
                                    Msg::ModeChanged(receiver_id, Mode::new(sel.value()))
                                },
                                _ => { Msg::None }
                            } )>
                        {
                            for RECEIVER_MODES.iter().map(|mode| {
                                html! { <option selected=if mode == &receiver.mode { true } else { false }>{ mode.mode() }</option> }
                            })
                        }
                    </select>
                </div>
            </form>
        }
    }

    fn decimal_mark(&self, s: String) -> String {
        let bytes: Vec<_> = s.bytes().rev().collect();
        let chunks: Vec<_> = bytes.chunks(3).map(|chunk| str::from_utf8(chunk).unwrap()).collect();
        let result: Vec<_> = chunks.join(",").bytes().rev().collect();
        String::from_utf8(result).unwrap()
    }
}