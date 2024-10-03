use std::os::fd::IntoRawFd;
use crate::frame::{object::Object, vulkan::Vulkan};
use crate::predictor::Controller;
use std::{cell::RefCell, rc::Rc, thread, time::Duration};
use wayland_client::{
    globals::{registry_queue_init, Global, GlobalList, GlobalListContents},
    protocol::{wl_output::WlOutput, wl_registry::WlRegistry},
    Connection, Dispatch, EventQueue, QueueHandle,
};

use wayland_protocols_wlr::export_dmabuf::v1::client::{
    zwlr_export_dmabuf_frame_v1::ZwlrExportDmabufFrameV1,
    zwlr_export_dmabuf_frame_v1::{CancelReason, Event},
    zwlr_export_dmabuf_manager_v1::ZwlrExportDmabufManagerV1,
};

use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1, zxdg_output_v1::Event::Description, zxdg_output_v1::ZxdgOutputV1,
};


const DELAY_SUCCESS: Duration = Duration::from_millis(100);
const DELAY_FAILURE: Duration = Duration::from_millis(1000);

#[derive(Clone)]
pub struct Capturer {
    event_queue: Rc<RefCell<EventQueue<Capturer>>>,
    globallists: Rc<GlobalListContents>,
    globals: Rc<GlobalList>,
    dmabuf_manager: ZwlrExportDmabufManagerV1,
    vulkan: Rc<Vulkan>,
    registry: WlRegistry,
    xdg_output_manager: ZxdgOutputManagerV1,
}

impl super::Capturer for Capturer {
    fn run(&self, output_name: &str, controller: Controller) {
        let controller = Rc::new(RefCell::new(controller));

        let cloned_list = self.globallists.clone_list();

        let unwraped_list = Rc::try_unwrap(self.globallists).unwrap();

        // Obtain the queue handle from your event queue
        let queue_handle: &QueueHandle<Capturer> = &self.event_queue.borrow().handle();

        cloned_list
            .iter()
            .filter(|Global| Global.interface == "wl_output")
            .for_each(|Global| {
                let output = Rc::new(self.registry.bind::<WlOutput, GlobalListContents, Capturer>(
                    Global.name,  // name
                    1,            // version
                    queue_handle, // queue handle
                    unwraped_list,    // user data
                ));
                let capturer = Rc::new(self.clone());
                let controller = controller.clone();
                let desired_output = output_name.to_string();
                self.xdg_output_manager
                    .get_xdg_output(&output, queue_handle, unwraped_list)
                    .quick_assign(move |_, event, _| match event {
                        Description { description } if description.contains(&desired_output) => {
                            log::debug!(
                                "Using output '{}' for config '{}'",
                                description,
                                desired_output,
                            );
                            capturer
                                .clone()
                                .capture_frame(controller.clone(), output.clone());
                        }
                        _ => {}
                    });
            });

        loop {
            self.event_queue
                .borrow_mut()
                .dispatch(&mut (), |_, _, _| {})
                .expect("Error running wlroots capturer main loop");
        }
    }
}


impl Default for Capturer {
    fn default() -> Self {
        let display = Connection::connect_to_env().unwrap();
        let mut event_queue = display.new_event_queue();
        let attached_display = display.display();

        // Create the QueueHandle
        let queue_handle: QueueHandle<Capturer> = event_queue.handle();

        let globals = GlobalList::new(&attached_display);
        let globallists = globals.clone_list(); // Store the GlobalListContents

        // Update the get_registry call
        let registry = attached_display.get_registry(&queue_handle, &globallists);

        // Instead of passing &mut self, we can use a local mutable variable
        let mut capturer_instance = Self {
            event_queue: Rc::new(RefCell::new(event_queue)),
            globallists,
            globals,
            registry,
            dmabuf_manager: globals
                .instantiate_exact::<ZwlrExportDmabufManagerV1>(1)
                .expect("Unable to init export_dmabuf_manager"),
            xdg_output_manager: globals
                .instantiate_exact::<ZxdgOutputManagerV1>(3)
                .expect("Unable to init xdg_output_manager"),
            vulkan: Rc::new(Vulkan::new().expect("Unable to initialize Vulkan")),
        };

        // Now you can roundtrip using the mutable reference
        capturer_instance.event_queue
            .borrow_mut()
            .roundtrip(&mut capturer_instance)
            .unwrap();

        capturer_instance // Return the instance
    }
}

impl Capturer {
    fn capture_frame(self: Rc<Self>, controller: Rc<RefCell<Controller>>, output: Rc<WlOutput>) {
        let mut frame = Object::default();

        // Obtain the queue handle from your event queue
        let queue_handle: &QueueHandle<Capturer> = &self.event_queue.borrow().handle();

        // Capture the frame using the dmabuf_manager
        self.dmabuf_manager
            .capture_output(
                0,                            // overlay_cursor
                &output,                      // output
                queue_handle,                 // queue handle
                Rc::try_unwrap(self.globallists).unwrap() // dereference Rc to get GlobalListContents
            );
            .quick_assign(move |data, event, _| match event {
                Event::Frame {
                    width,
                    height,
                    num_objects,
                    ..
                } => {
                    frame.set_metadata(width, height, num_objects);
                }

                Event::Object { index, fd, size, .. } => {
    let fd_raw = fd.into_raw_fd(); // Convert OwnedFd to i32
                    frame.set_object(index, fd_raw, size);
                }

                Event::Ready { .. } => {
                    let luma = self
                        .vulkan
                        .luma_percent(&frame)
                        .expect("Unable to compute luma percent");

                    controller.borrow_mut().adjust(luma);

                    data.destroy();

                    thread::sleep(DELAY_SUCCESS);
                    self.clone().capture_frame(controller.clone(), output.clone());
                }

                Event::Cancel { reason } => {
                    data.destroy();

                    if reason == wayland_client::WEnum::Value(CancelReason::Permanent) {
                        panic!("Frame was cancelled due to a permanent error. If you just disconnected screen, this is not implemented yet.");
                    } else {
                        log::error!("Frame was cancelled due to a temporary error, will try again.");
                        thread::sleep(DELAY_FAILURE);
                        self.clone().capture_frame(controller.clone(), output.clone());
                    }
                }

                _ => unreachable!(),
            });
    }
}

// Implement the Dispatch trait for handling WlOutput events
impl Dispatch<WlOutput, GlobalListContents> for Capturer {
    fn event(
        _state: &mut Self,
        _proxy: &WlOutput,
        event: <WlOutput as wayland_client::Proxy>::Event,
        _udata: &GlobalListContents, // Replaced MyUserData with GlobalListContents
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wayland_client::protocol::wl_output::Event::Geometry {
                x,
                y,
                physical_width,
                physical_height,
                ..
            } => {
                log::info!(
                    "Output has geometry: {}x{} at position ({}, {})",
                    physical_width,
                    physical_height,
                    x,
                    y
                );
            }

            wayland_client::protocol::wl_output::Event::Mode {
                width,
                height,
                refresh,
                ..
            } => {
                log::info!(
                    "Output has mode: {}x{} @ {} Hz",
                    width,
                    height,
                    refresh as f32 / 1000.0
                );
            }

            _ => {
                log::info!("Unhandled wl_output event");
            }
        }
    }
}

impl Dispatch<ZwlrExportDmabufFrameV1, GlobalListContents> for Capturer {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrExportDmabufFrameV1,
        event: wayland_protocols_wlr::export_dmabuf::v1::client::zwlr_export_dmabuf_frame_v1::Event,
        _udata: &GlobalListContents, // Replaced MyUserData with GlobalListContents
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {

            wayland_protocols_wlr::export_dmabuf::v1::client::zwlr_export_dmabuf_frame_v1::Event::Frame { width, height, num_objects, .. } => {
                log::info!(
                    "Received a frame: {}x{}, {} objects",
                    width,
                    height,
                    num_objects
                );
            }

            wayland_protocols_wlr::export_dmabuf::v1::client::zwlr_export_dmabuf_frame_v1::Event::Cancel { reason } => {
                log::error!(
                    "Frame capture was cancelled, reason: {:?}",
                    reason
                );
            }

            _ => {
                log::info!("Unhandled dmabuf event");
            }
        }
    }
}

impl Dispatch<ZxdgOutputV1, GlobalListContents> for Capturer {
    fn event(
        _state: &mut Self,
        _proxy: &ZxdgOutputV1,
        event: <ZxdgOutputV1 as wayland_client::Proxy>::Event,
        _udata: &GlobalListContents, // Now using GlobalListContents
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::Event::LogicalPosition { x, y } => {
                log::info!(
                    "Received logical position: ({}, {})", x, y
                );
            }

            wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::Event::LogicalSize { width, height } => {
                log::info!(
                    "Received logical size: {}x{}", width, height
                );
            }

            wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::Event::Done => {
                log::info!("Received done event");
            }

            _ => {
                log::warn!("Unhandled xdg_output event");
            }
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for Capturer {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        event: <WlRegistry as wayland_client::Proxy>::Event,
        _globals: &GlobalListContents, // Now we pass GlobalListContents instead of custom user data
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            // Handle when a new global object is announced
            wayland_client::protocol::wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                log::info!(
                    "New global: {} (version: {}, name: {})",
                    interface,
                    version,
                    name
                );

                // Here you can bind specific globals when announced
                if interface == "wl_output" {
                    // You can bind the global here if needed
                    log::info!("Binding wl_output global (name: {})", name);
                    // Example: self.bind_output(name, version); // Actual binding would happen here
                }
            }

            // Handle when a global object is removed
            wayland_client::protocol::wl_registry::Event::GlobalRemove { name } => {
                log::info!("Global removed: name={}", name);
            }

            _ => {
                log::warn!("Unhandled wl_registry event: {:?}", event);
            }
        }
    }
}
