![linkerd2][logo]

<!-- TODO [![Build Status][ci-badge]][ci] -->
[![GitHub license][license-badge]](LICENSE)
[![Slack Status][slack-badge]][slack]

This repo contains the transparent proxy component of [Linkerd2][linkerd2].
While the Linkerd2 proxy is heavily influenced by [the Linkerd 1.X
proxy][linkerd1], it comprises an entirely new codebase implemented in the
[Rust programming language][rust].

This proxy's features include:

* Transparent, zero-config proxying for HTTP, HTTP/2, and arbitrary TCP protocols.
* Automatic [Prometheus][prom] metrics export for HTTP and TCP traffic;
* Transparent, zero-config WebSocket proxying;
* Automatic, latency-aware, layer-7 [load balancing][loadbalancing];
* Automatic layer-4 load balancing for non-HTTP traffic;
* Automatic TLS (experimental);
* An on-demand diagnostic `tap` API.

This proxy is primarily intended to run on Linux in containerized
environments like [Kubernetes][k8s], though it may also work on other
Unix-like systems (like macOS).

The proxy supports service discovery via DNS and the [linkerd2
`Destination` gRPC API][linkerd2-proxy-api].

The Linkerd project is hosted by the Cloud Native Computing Foundation
([CNCF][cncf]).


## Building the project

A `Makefile` is provided to automate most build tasks. It provides the
following targets:

* `make build` -- Compiles the proxy on your local system using `cargo`
* `make clean` -- Cleans the build target on the local system using `cargo clean`
* `make test` -- Runs unit and integration tests on your local system using `cargo`
* `make test-flakey` -- Runs _all_ tests, including those that may fail spuriously
* `make package` -- Builds a tarball at
  `target/release/linkerd2-proxy-${PACKAGE_VERSION}.tar.gz`. If
  `PACKAGE_VERSION` is not set in the environment, the local git SHA is used.
* `make docker` -- Builds a Docker container image that can be used for testing.
   If the `DOCKER_TAG` environment variable is set, the image is given this
   name. Otherwise, the image is not named.

### Cargo

Usually, [Cargo][cargo], Rust's package manager, is used to build and test this
project. If you don't have Cargo installed, we suggest getting it via
https://rustup.rs/.

### Repository Structure

This project is broken into many small libraries, or _crates_, so that
components may be compiled & tested independently. The following crate
targets are especially important:

* [`linkerd2-proxy`] contains the proxy executable;
* [`linkerd2-app-integration`] contains the proxy's integration tests;
* [`linkerd2-app`] bundles the [`linkerd2-app-inbound`] and [`linkerd2-app-outbound`] crates so that they may be run by the executable or integration tests.

[`linkerd2-proxy`]: linkerd2-proxy
[`linkerd2-app`]: linkerd/app
[`linkerd2-app-integration`]: linkerd/app/integration
[`linkerd2-app-inbound`]: linkerd/app/inbound
[`linkerd2-app-outbound`]: linkerd/app/outbound

## Code of conduct

This project is for everyone. We ask that our users and contributors take a few
minutes to review our [code of conduct][coc].


## Security

We test our code by way of fuzzing and this is described in [FUZZING.md](/docs/FUZZING.md).

A third party security audit focused on fuzzing Linkerd2-proxy was performed by Ada Logics in 2021. The full report is available [here](docs/reports/linkerd2-proxy-fuzzing-report.pdf).


## License

linkerd2-proxy is copyright 2018 the linkerd2-proxy authors. All rights reserved.

Licensed under the Apache License, Version 2.0 (the "License"); you may not use
these files except in compliance with the License. You may obtain a copy of the
License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software distributed
under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
CONDITIONS OF ANY KIND, either express or implied. See the License for the
specific language governing permissions and limitations under the License.


<!-- refs -->
[cargo]: https://github.com/rust-lang/cargo/
[ci]: https://travis-ci.org/linkerd/linkerd2-proxy
<!-- TODO [ci-badge]: https://travis-ci.org/linkerd/linkerd2-proxy.svg?branch=master -->
[cncf]: https://cncf.io/
[coc]: https://github.com/linkerd/linkerd/wiki/Linkerd-code-of-conduct
[k8s]: https://kubernetes.io/
[license-badge]: https://img.shields.io/github/license/linkerd/linkerd2.svg
[linkerd1]: https://github.com/linkerd/linkerd
[linkerd2]: https://github.com/linkerd/linkerd2
[linkerd2-proxy-api]: https://github.com/linkerd/linkerd2-proxy-api
[loadbalancing]: https://linkerd.io/features/load-balancing/
[logo]: https://user-images.githubusercontent.com/9226/33582867-3e646e02-d90c-11e7-85a2-2e238737e859.png
[prom]: https://prometheus.io/
[rust]: https://www.rust-lang.org/
[slack-badge]: https://slack.linkerd.io/badge.svg
[slack]: https://slack.linkerd.io
[twitter]: https://twitter.com/linkerd/
