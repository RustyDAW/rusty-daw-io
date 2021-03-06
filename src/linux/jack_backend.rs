use log::{debug, info, warn};

use crate::{
    AudioBus, AudioBusBuffer, AudioDeviceInfo, AudioServerInfo, BufferSizeRange, Config,
    DeviceIndex, FatalErrorHandler, FatalStreamError, MidiController, MidiControllerBuffer,
    MidiDeviceInfo, MidiServerInfo, ProcessInfo, RtProcessHandler, SpawnRtThreadError, StreamInfo,
};

pub fn refresh_audio_server(server: &mut AudioServerInfo) {
    info!("Refreshing list of available Jack audio devices...");

    server.devices.clear();

    match jack::Client::new("rustydaw_io_dummy_client", jack::ClientOptions::empty()) {
        Ok((client, _status)) => {
            let system_audio_in_ports: Vec<String> = client.ports(
                None,
                Some("32 bit float mono audio"),
                jack::PortFlags::IS_OUTPUT,
            );
            let system_audio_out_ports: Vec<String> = client.ports(
                None,
                Some("32 bit float mono audio"),
                jack::PortFlags::IS_INPUT,
            );

            if system_audio_out_ports.len() == 0 {
                // This crate only allows devices with playback.

                server.available = false;

                warn!("Jack server is unavailable: Jack system device has no available audio outputs.");
            } else {
                // Find index of default in ports.
                let mut default_in_port = 0; // Fallback to first available port.
                for (i, port) in system_audio_in_ports.iter().enumerate() {
                    if port == "system:capture_1" {
                        default_in_port = i;
                        break;
                    }
                }

                // Find index of default out left port.
                let mut default_out_port_left = 0; // Fallback to first available port.
                for (i, port) in system_audio_out_ports.iter().enumerate() {
                    if port == "system:playback_1" {
                        default_out_port_left = i;
                        break;
                    }
                }

                // Find index of default out right port.
                let mut default_out_port_right = 1.min(system_audio_out_ports.len() - 1); // Fallback to second available port if stereo, first if mono.
                for (i, port) in system_audio_out_ports.iter().enumerate() {
                    if port == "system:playback_2" {
                        default_out_port_right = i;
                        break;
                    }
                }

                // Jack only ever has one "device".
                server.devices.push(AudioDeviceInfo {
                    name: String::from("Jack Device"),
                    in_ports: system_audio_in_ports,
                    out_ports: system_audio_out_ports,
                    sample_rates: vec![client.sample_rate() as u32], // Only one sample rate is available.
                    buffer_size_range: BufferSizeRange {
                        // Only one buffer size is available.
                        min: client.buffer_size() as u32,
                        max: client.buffer_size() as u32,
                    },

                    default_in_port,
                    default_out_port_left,
                    default_out_port_right,
                    default_sample_rate_index: 0, // Only one sample rate is available.
                    default_buffer_size: client.buffer_size() as u32, // Only one buffer size is available.
                });

                server.available = true;
            }
        }
        Err(e) => {
            server.available = false;

            info!("Jack server is unavailable: {}", e);
        }
    }
}

pub fn refresh_midi_server(server: &mut MidiServerInfo) {
    info!("Refreshing list of available Jack MIDI devices...");

    server.in_devices.clear();
    server.out_devices.clear();

    match jack::Client::new("rustydaw_io_dummy_client", jack::ClientOptions::empty()) {
        Ok((client, _status)) => {
            let system_midi_in_ports: Vec<String> =
                client.ports(None, Some("8 bit raw midi"), jack::PortFlags::IS_OUTPUT);
            let system_midi_out_ports: Vec<String> =
                client.ports(None, Some("8 bit raw midi"), jack::PortFlags::IS_INPUT);

            for system_port_name in system_midi_in_ports.iter() {
                server.in_devices.push(MidiDeviceInfo {
                    name: system_port_name.clone(),
                });
            }

            for system_port_name in system_midi_out_ports.iter() {
                server.out_devices.push(MidiDeviceInfo {
                    name: system_port_name.clone(),
                });
            }

            // Find index of default in port.
            let mut default_in_port = 0; // Fallback to first available port.
            for (i, port) in system_midi_in_ports.iter().enumerate() {
                // "system:midi_capture_1" is usually Jack's built-in `Midi-Through` device.
                // What we usually want is first available port of the user's hardware MIDI controller, which is
                // commonly mapped to "system:midi_capture_2".
                if port == "system:midi_capture_2" {
                    default_in_port = i;
                    break;
                }
            }

            server.default_in_port = default_in_port;

            server.available = true;
        }
        Err(e) => {
            server.available = false;

            info!("Jack server is unavailable: {}", e);
        }
    }
}

pub struct JackRtThreadHandle<P: RtProcessHandler, E: FatalErrorHandler> {
    _async_client: jack::AsyncClient<JackNotificationHandler<E>, JackProcessHandler<P>>,
}

pub fn spawn_rt_thread<P: RtProcessHandler, E: FatalErrorHandler>(
    config: &Config,
    mut rt_process_handler: P,
    fatal_error_handler: E,
    use_client_name: Option<String>,
) -> Result<(StreamInfo, JackRtThreadHandle<P, E>), SpawnRtThreadError> {
    info!("Spawning Jack thread...");

    let client_name = use_client_name.unwrap_or(String::from("rusty-daw-io"));

    info!("Registering Jack client with name {}", &client_name);

    let (client, _status) = jack::Client::new(&client_name, jack::ClientOptions::empty())?;

    // Find system ports

    let system_audio_in_ports: Vec<String> = client.ports(
        None,
        Some("32 bit float mono audio"),
        jack::PortFlags::IS_OUTPUT,
    );
    let system_audio_out_ports: Vec<String> = client.ports(
        None,
        Some("32 bit float mono audio"),
        jack::PortFlags::IS_INPUT,
    );

    // Register new ports.

    let mut audio_in_ports = Vec::<jack::Port<jack::AudioIn>>::new();
    let mut audio_in_port_names = Vec::<String>::new();
    let mut audio_in_connected_port_names = Vec::<String>::new();
    let mut audio_in_busses = Vec::<AudioBus>::new();
    for (bus_i, bus) in config.audio_in_busses.iter().enumerate() {
        if bus.system_ports.len() == 0 {
            return Err(SpawnRtThreadError::NoSystemPortsGiven(bus.id.clone()));
        }

        audio_in_busses.push(AudioBus {
            id_name: bus.id.clone(),
            id_index: DeviceIndex::new(bus_i),
            system_device: String::from("Jack"),
            system_half_duplex_device: None,
            system_ports: bus.system_ports.clone(),
            channels: bus.system_ports.len() as u16,
        });

        for (i, system_port) in bus.system_ports.iter().enumerate() {
            if !system_audio_in_ports.contains(&system_port) {
                return Err(SpawnRtThreadError::SystemPortNotFound(
                    system_port.clone(),
                    bus.id.clone(),
                ));
            }

            let user_port_name = format!("{}_{}", &bus.id, i + 1);
            let user_port = client.register_port(&user_port_name, jack::AudioIn::default())?;

            audio_in_port_names.push(user_port.name()?);
            audio_in_connected_port_names.push(system_port.clone());
            audio_in_ports.push(user_port);
        }
    }

    let mut audio_out_ports = Vec::<jack::Port<jack::AudioOut>>::new();
    let mut audio_out_port_names = Vec::<String>::new();
    let mut audio_out_connected_port_names = Vec::<String>::new();
    let mut audio_out_busses = Vec::<AudioBus>::new();
    for (bus_i, bus) in config.audio_out_busses.iter().enumerate() {
        if bus.system_ports.len() == 0 {
            return Err(SpawnRtThreadError::NoSystemPortsGiven(bus.id.clone()));
        }

        audio_out_busses.push(AudioBus {
            id_name: bus.id.clone(),
            id_index: DeviceIndex::new(bus_i),
            system_device: String::from("Jack"),
            system_half_duplex_device: None,
            system_ports: bus.system_ports.clone(),
            channels: bus.system_ports.len() as u16,
        });

        for (i, system_port) in bus.system_ports.iter().enumerate() {
            if !system_audio_out_ports.contains(&system_port) {
                return Err(SpawnRtThreadError::SystemPortNotFound(
                    system_port.clone(),
                    bus.id.clone(),
                ));
            }

            let user_port_name = format!("{}_{}", &bus.id, i + 1);
            let user_port = client.register_port(&user_port_name, jack::AudioOut::default())?;

            audio_out_port_names.push(user_port.name()?);
            audio_out_connected_port_names.push(system_port.clone());
            audio_out_ports.push(user_port);
        }
    }

    let mut midi_in_ports = Vec::<jack::Port<jack::MidiIn>>::new();
    let mut midi_in_port_names = Vec::<String>::new();
    let mut midi_in_connected_port_names = Vec::<String>::new();
    let mut midi_in_controllers = Vec::<MidiController>::new();

    let mut midi_out_ports = Vec::<jack::Port<jack::MidiOut>>::new();
    let mut midi_out_port_names = Vec::<String>::new();
    let mut midi_out_connected_port_names = Vec::<String>::new();
    let mut midi_out_controllers = Vec::<MidiController>::new();

    if let Some(midi_server) = &config.midi_server {
        if midi_server == "Jack" {
            for (controller_i, controller) in config.midi_in_controllers.iter().enumerate() {
                let system_port_name = &controller.system_port;

                midi_in_controllers.push(MidiController {
                    id_name: controller.id.clone(),
                    id_index: DeviceIndex::new(controller_i),
                    system_port: String::from(system_port_name),
                });

                let port = client.register_port(&controller.id, jack::MidiIn::default())?;

                midi_in_port_names.push(port.name()?);
                midi_in_connected_port_names.push(String::from(system_port_name));
                midi_in_ports.push(port);
            }

            for (controller_i, controller) in config.midi_out_controllers.iter().enumerate() {
                let system_port_name = &controller.system_port;

                midi_out_controllers.push(MidiController {
                    id_name: controller.id.clone(),
                    id_index: DeviceIndex::new(controller_i),
                    system_port: String::from(system_port_name),
                });

                let port = client.register_port(&controller.id, jack::MidiOut::default())?;

                midi_out_port_names.push(port.name()?);
                midi_out_connected_port_names.push(String::from(system_port_name));
                midi_out_ports.push(port);
            }
        }
    }

    let sample_rate = client.sample_rate() as u32;
    let max_audio_buffer_size = client.buffer_size() as u32;

    let stream_info = StreamInfo {
        server_name: String::from("Jack"),
        audio_in: audio_in_busses,
        audio_out: audio_out_busses,
        midi_in: midi_in_controllers,
        midi_out: midi_out_controllers,
        sample_rate: sample_rate as u32,
        max_audio_buffer_size,
    };

    rt_process_handler.init(&stream_info);

    let process = JackProcessHandler::new(
        rt_process_handler,
        audio_in_ports,
        audio_out_ports,
        midi_in_ports,
        midi_out_ports,
        stream_info.clone(),
        max_audio_buffer_size,
    );

    info!("Activating Jack client...");

    // Activate the client, which starts the processing.
    let async_client = client.activate_async(
        JackNotificationHandler {
            fatal_error_handler: Some(fatal_error_handler),
        },
        process,
    )?;

    // Try to automatically connect to system inputs/outputs.

    for (in_port, system_in_port) in audio_in_port_names
        .iter()
        .zip(audio_in_connected_port_names)
    {
        async_client
            .as_client()
            .connect_ports_by_name(&system_in_port, in_port)?;
    }
    for (out_port, system_out_port) in audio_out_port_names
        .iter()
        .zip(audio_out_connected_port_names)
    {
        async_client
            .as_client()
            .connect_ports_by_name(out_port, &system_out_port)?;
    }

    for (in_port, system_in_port) in midi_in_port_names.iter().zip(midi_in_connected_port_names) {
        async_client
            .as_client()
            .connect_ports_by_name(&system_in_port, in_port)?;
    }
    for (out_port, system_out_port) in midi_out_port_names
        .iter()
        .zip(midi_out_connected_port_names)
    {
        async_client
            .as_client()
            .connect_ports_by_name(out_port, &system_out_port)?;
    }

    info!(
        "Successfully spawned Jack thread. Sample rate: {}, Max audio buffer size: {}",
        sample_rate, max_audio_buffer_size
    );

    Ok((
        stream_info,
        JackRtThreadHandle {
            _async_client: async_client,
        },
    ))
}

struct JackProcessHandler<P: RtProcessHandler> {
    rt_process_handler: P,

    audio_in_ports: Vec<jack::Port<jack::AudioIn>>,
    audio_out_ports: Vec<jack::Port<jack::AudioOut>>,

    audio_in_buffers: Vec<AudioBusBuffer>,
    audio_out_buffers: Vec<AudioBusBuffer>,

    midi_in_ports: Vec<jack::Port<jack::MidiIn>>,
    midi_out_ports: Vec<jack::Port<jack::MidiOut>>,

    midi_in_buffers: Vec<MidiControllerBuffer>,
    midi_out_buffers: Vec<MidiControllerBuffer>,

    stream_info: StreamInfo,
    max_audio_buffer_size: usize,
}

impl<P: RtProcessHandler> JackProcessHandler<P> {
    fn new(
        rt_process_handler: P,
        audio_in_ports: Vec<jack::Port<jack::AudioIn>>,
        audio_out_ports: Vec<jack::Port<jack::AudioOut>>,
        midi_in_ports: Vec<jack::Port<jack::MidiIn>>,
        midi_out_ports: Vec<jack::Port<jack::MidiOut>>,
        stream_info: StreamInfo,
        max_audio_buffer_size: u32,
    ) -> Self {
        let mut audio_in_buffers = Vec::<AudioBusBuffer>::new();
        let mut audio_out_buffers = Vec::<AudioBusBuffer>::new();

        for bus in stream_info.audio_in.iter() {
            audio_in_buffers.push(AudioBusBuffer::new(bus.channels, max_audio_buffer_size))
        }
        for bus in stream_info.audio_out.iter() {
            audio_out_buffers.push(AudioBusBuffer::new(bus.channels, max_audio_buffer_size))
        }

        let mut midi_in_buffers = Vec::<MidiControllerBuffer>::new();
        let mut midi_out_buffers = Vec::<MidiControllerBuffer>::new();

        for _ in 0..stream_info.midi_in.len() {
            midi_in_buffers.push(MidiControllerBuffer::new())
        }
        for _ in 0..stream_info.midi_out.len() {
            midi_out_buffers.push(MidiControllerBuffer::new())
        }

        Self {
            rt_process_handler,
            audio_in_ports,
            audio_out_ports,
            audio_in_buffers,
            audio_out_buffers,
            midi_in_ports,
            midi_out_ports,
            midi_in_buffers,
            midi_out_buffers,
            stream_info,
            max_audio_buffer_size: max_audio_buffer_size as usize,
        }
    }
}

impl<P: RtProcessHandler> jack::ProcessHandler for JackProcessHandler<P> {
    fn process(&mut self, _: &jack::Client, ps: &jack::ProcessScope) -> jack::Control {
        let mut audio_frames = 0;

        // Collect Audio Inputs

        let mut port = 0; // Ports are in order.
        for audio_buffer in self.audio_in_buffers.iter_mut() {
            for channel in audio_buffer.channel_buffers.iter_mut() {
                let port_slice = self.audio_in_ports[port].as_slice(ps);

                audio_frames = port_slice.len();

                // Sanity check.
                if audio_frames > self.max_audio_buffer_size {
                    warn!("Warning: Jack sent a buffer size of {} when the max buffer size was said to be {}", audio_frames, self.max_audio_buffer_size);
                }

                // The compiler should in-theory optimize by not filling in zeros before copying
                // the slice. This should never allocate because each buffer was given a capacity of
                // the maximum buffer size that jack will send.
                channel.resize(audio_frames, 0.0);
                channel.copy_from_slice(port_slice);

                port += 1;
            }

            audio_buffer.frames = audio_frames;
        }

        if self.audio_in_buffers.len() == 0 {
            // Check outputs for number of frames instead.
            if let Some(out_port) = self.audio_out_ports.first_mut() {
                audio_frames = out_port.as_mut_slice(ps).len();
            }
        }

        // Clear Audio Outputs

        for audio_buffer in self.audio_out_buffers.iter_mut() {
            audio_buffer.clear_and_resize(audio_frames);
        }

        // Collect MIDI Inputs

        for (midi_buffer, port) in self
            .midi_in_buffers
            .iter_mut()
            .zip(self.midi_in_ports.iter())
        {
            midi_buffer.clear();

            for event in port.iter(ps) {
                if let Err(e) = midi_buffer.push_raw(event.time, event.bytes) {
                    warn!(
                        "Warning: Dropping midi event because of the push error: {}",
                        e
                    );
                }
            }
        }

        // Clear MIDI Outputs

        for midi_buffer in self.midi_out_buffers.iter_mut() {
            midi_buffer.clear();
        }

        self.rt_process_handler.process(ProcessInfo {
            audio_in: self.audio_in_buffers.as_slice(),
            audio_out: self.audio_out_buffers.as_mut_slice(),
            audio_frames,

            midi_in: self.midi_in_buffers.as_slice(),
            midi_out: self.midi_out_buffers.as_mut_slice(),

            sample_rate: self.stream_info.sample_rate,
        });

        // TODO: Properly mix outputs in the case where a system port is connected to more than one bus/controller.

        // Copy processed data to Audio Outputs

        let mut port = 0; // Ports are in order.
        for audio_buffer in self.audio_out_buffers.iter() {
            for channel in audio_buffer.channel_buffers.iter() {
                let port_slice = self.audio_out_ports[port].as_mut_slice(ps);

                // Just in case the user resized the output buffer for some reason.
                let len = channel.len().min(port_slice.len());
                if len != audio_frames {
                    warn!(
                        "Warning: An audio output buffer was resized from {} to {} by the user",
                        audio_frames, len
                    );
                }

                &mut port_slice[0..len].copy_from_slice(&channel[0..len]);

                port += 1;
            }
        }

        // Copy processed data to MIDI Outputs

        for (midi_buffer, port) in self
            .midi_out_buffers
            .iter()
            .zip(self.midi_out_ports.iter_mut())
        {
            let mut port_writer = port.writer(ps);

            for event in midi_buffer.events() {
                if let Err(e) = port_writer.write(&jack::RawMidi {
                    time: event.delta_frames,
                    bytes: &event.data(),
                }) {
                    warn!("Warning: Could not copy midi data to Jack output: {}", e);
                }
            }
        }

        jack::Control::Continue
    }
}

struct JackNotificationHandler<E: FatalErrorHandler> {
    fatal_error_handler: Option<E>,
}

impl<E: FatalErrorHandler> jack::NotificationHandler for JackNotificationHandler<E> {
    fn thread_init(&self, _: &jack::Client) {
        debug!("JACK: thread init");
    }

    fn shutdown(&mut self, status: jack::ClientStatus, reason: &str) {
        let msg = format!(
            "JACK: shutdown with status {:?} because \"{}\"",
            status, reason
        );

        info!("{}", msg);

        if let Some(fatal_error_handler) = self.fatal_error_handler.take() {
            fatal_error_handler.fatal_stream_error(FatalStreamError::AudioServerDisconnected(msg))
        }
    }

    fn freewheel(&mut self, _: &jack::Client, is_enabled: bool) {
        debug!(
            "JACK: freewheel mode is {}",
            if is_enabled { "on" } else { "off" }
        );
    }

    fn sample_rate(&mut self, _: &jack::Client, srate: jack::Frames) -> jack::Control {
        debug!("JACK: sample rate changed to {}", srate);
        jack::Control::Continue
    }

    fn client_registration(&mut self, _: &jack::Client, name: &str, is_reg: bool) {
        debug!(
            "JACK: {} client with name \"{}\"",
            if is_reg { "registered" } else { "unregistered" },
            name
        );
    }

    fn port_registration(&mut self, _: &jack::Client, port_id: jack::PortId, is_reg: bool) {
        debug!(
            "JACK: {} port with id {}",
            if is_reg { "registered" } else { "unregistered" },
            port_id
        );
    }

    fn port_rename(
        &mut self,
        _: &jack::Client,
        port_id: jack::PortId,
        old_name: &str,
        new_name: &str,
    ) -> jack::Control {
        debug!(
            "JACK: port with id {} renamed from {} to {}",
            port_id, old_name, new_name
        );
        jack::Control::Continue
    }

    fn ports_connected(
        &mut self,
        _: &jack::Client,
        port_id_a: jack::PortId,
        port_id_b: jack::PortId,
        are_connected: bool,
    ) {
        debug!(
            "JACK: ports with id {} and {} are {}",
            port_id_a,
            port_id_b,
            if are_connected {
                "connected"
            } else {
                "disconnected"
            }
        );
    }

    fn graph_reorder(&mut self, _: &jack::Client) -> jack::Control {
        debug!("JACK: graph reordered");
        jack::Control::Continue
    }

    fn xrun(&mut self, _: &jack::Client) -> jack::Control {
        warn!("JACK: xrun occurred");
        jack::Control::Continue
    }

    fn latency(&mut self, _: &jack::Client, mode: jack::LatencyType) {
        debug!(
            "JACK: {} latency has changed",
            match mode {
                jack::LatencyType::Capture => "capture",
                jack::LatencyType::Playback => "playback",
            }
        );
    }
}

impl From<jack::Error> for SpawnRtThreadError {
    fn from(e: jack::Error) -> Self {
        SpawnRtThreadError::PlatformSpecific(Box::new(e))
    }
}
