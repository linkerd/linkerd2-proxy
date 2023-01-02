use linkerd_app_core::svc;
use linkerd_distribute::CacheNewDistribute;
use linkerd_router::NewRoute;

fn foo() {
    let _ = svc::layers()
        .push(CacheNewDistribute::layer())
        .push(NewRoute::layer(svc::layers()));
}
