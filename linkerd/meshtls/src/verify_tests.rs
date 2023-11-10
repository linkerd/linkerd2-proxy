#[macro_export]
macro_rules! generate_verify_id_tests {
    ($cert_type:ty, $vec_to_cert: ident) => {
        use super::verify_id;
        use linkerd_identity as id;
        use rcgen::generate_simple_self_signed;
        use std::str::FromStr;

        fn generate_cert_with_names(names: Vec<&str>) -> $cert_type {
            let sans: Vec<String> = names.into_iter().map(|s| s.into()).collect();

            let cert_data = generate_simple_self_signed(sans)
                .expect("should generate cert")
                .serialize_der()
                .expect("should serialize");

            $vec_to_cert(cert_data)
        }

        #[test]
        fn cert_with_dns_san_matches_dns_id() {
            let dns_name = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
            let cert = generate_cert_with_names(vec![dns_name]);
            let id = id::Id::from_str(dns_name).expect("should parse DNS id");
            assert!(verify_id(&cert, &id).is_ok());
        }

        #[test]
        fn cert_with_dns_san_does_not_match_dns_id() {
            let dns_name_cert = vec!["foo.ns1.serviceaccount.identity.linkerd.cluster.local"];
            let dns_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";

            let cert = generate_cert_with_names(dns_name_cert);
            let id = id::Id::from_str(dns_name).expect("should parse DNS id");
            assert!(verify_id(&cert, &id).is_err());
        }

        #[test]
        fn cert_with_uri_san_does_not_match_dns_id() {
            let uri_name_cert = vec!["spiffe://some-trust-comain/some-system/some-component"];
            let dns_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";

            let cert = generate_cert_with_names(uri_name_cert);
            let id = id::Id::from_str(dns_name).expect("should parse DNS id");
            assert!(verify_id(&cert, &id).is_err());
        }

        #[test]
        fn cert_with_no_san_does_not_verify_for_dns_id() {
            let dns_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
            let cert = generate_cert_with_names(vec![]);
            let id = id::Id::from_str(dns_name).expect("should parse DNS id");
            assert!(verify_id(&cert, &id).is_err());
        }

        #[test]
        fn cert_with_dns_multiple_sans_one_matches_dns_id() {
            let foo_dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
            let bar_dns_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
            let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";

            let cert = generate_cert_with_names(vec![foo_dns_id, bar_dns_id, spiffe_id]);
            let id = id::Id::from_str(foo_dns_id).expect("should parse DNS id");
            assert!(verify_id(&cert, &id).is_ok());
        }

        #[test]
        fn cert_with_dns_multiple_sans_none_matches_dns_id() {
            let foo_dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
            let bar_dns_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
            let nar_dns_id = "nar.ns1.serviceaccount.identity.linkerd.cluster.local";
            let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";

            let cert = generate_cert_with_names(vec![foo_dns_id, bar_dns_id, spiffe_id]);
            let id = id::Id::from_str(nar_dns_id).expect("should parse DNS id");
            assert!(verify_id(&cert, &id).is_err());
        }
    };
}
