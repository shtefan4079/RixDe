const _: &str = include_str!("../surface-effects.xml");
const _: &str = include_str!("../capture-policy.xml");
const _: &str = include_str!("../toplevel-observer.xml");
const _: &str = include_str!("../remote-input.xml");
#[cfg(feature = "client")]
pub mod client {
    use wayland_client;
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("surface-effects.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("surface-effects.xml");
}
#[cfg(feature = "server")]
pub mod server {
    use wayland_server;
    use wayland_server::protocol::*;
    pub mod __interfaces {
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("surface-effects.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_server_code!("surface-effects.xml");
}

#[cfg(feature = "client")]
pub mod capture_client {
    use wayland_client;
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("capture-policy.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("capture-policy.xml");
}

#[cfg(feature = "server")]
pub mod capture_server {
    use wayland_server;
    use wayland_server::protocol::*;
    pub mod __interfaces {
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("capture-policy.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_server_code!("capture-policy.xml");
}

#[cfg(feature = "client")]
pub mod observer_client {
    use wayland_client;
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("toplevel-observer.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("toplevel-observer.xml");
}

#[cfg(feature = "server")]
pub mod observer_server {
    use wayland_server;
    use wayland_server::protocol::*;
    pub mod __interfaces {
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("toplevel-observer.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_server_code!("toplevel-observer.xml");
}

#[cfg(feature = "client")]
pub mod remote_input_client {
    use wayland_client;
    pub mod __interfaces {
        wayland_scanner::generate_interfaces!("remote-input.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("remote-input.xml");
}

#[cfg(feature = "server")]
pub mod remote_input_server {
    use wayland_server;
    pub mod __interfaces {
        wayland_scanner::generate_interfaces!("remote-input.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_server_code!("remote-input.xml");
}
