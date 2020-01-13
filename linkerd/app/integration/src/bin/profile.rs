#![deny(warnings)]
#![recursion_limit = "128"]
use linkerd2_app_integration::*;
use std::env;
use std::io::Read;
use std::net::TcpListener;

fn main() {
    match env::var_os("PROFILING_SUPPORT_SERVER") {
        Some(srv_addr) => {
            let addr: SocketAddr = srv_addr
                .to_str()
                .expect("PROFILING_SUPPORT_SERVER not a string")
                .parse()
                .expect("could not parse PROFILE_SUPPORT_SERVER (expects, eg., 127.0.0.1:8000)");

            let srv = server::mock_listening(addr.clone());
            let srv2 = server::mock_listening(addr.clone());
            let ctrl = controller::new()
                .destination_and_close("transparency.test.svc.cluster.local", srv.addr)
                .run();
            let env = TestEnv::new();
            let _proxy = proxy::new()
                .controller(ctrl)
                .outbound(srv)
                .inbound(srv2)
                .run_with_test_env_and_keep_ports(env);
            let listener = TcpListener::bind("127.0.0.1:7777").expect("could not bind");
            let (mut stream, _) = listener.accept().expect("did not accept");
            let mut buf = [0];
            stream
                .read_exact(&mut buf)
                .expect("could not read singal byte");
            assert_eq!(buf[0], 'F' as u8);
        }
        None => {} // skip this if not run from the profiling script
    }
}
