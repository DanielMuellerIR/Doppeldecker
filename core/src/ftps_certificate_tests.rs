use super::*;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose,
};

fn authority() -> (Certificate, Issuer<'static, KeyPair>) {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    let key = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    (cert, Issuer::new(params, key))
}

fn certificate(issuer: &Issuer<'_, KeyPair>, name: &str, first: i32, last: i32) -> Certificate {
    let mut params = CertificateParams::new(vec![name.into()]).unwrap();
    params.not_before = rcgen::date_time_ymd(first, 1, 1);
    params.not_after = rcgen::date_time_ymd(last, 1, 1);
    params
        .signed_by(&KeyPair::generate().unwrap(), issuer)
        .unwrap()
}

fn with_authority(
    mut verifier: PinnedCertificateVerifier,
    ca: &Certificate,
) -> PinnedCertificateVerifier {
    let mut roots = RootCertStore::empty();
    roots.add(ca.der().clone()).unwrap();
    verifier.webpki = Some(
        WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .unwrap(),
    );
    verifier
}

fn verify(
    verifier: &PinnedCertificateVerifier,
    cert: &Certificate,
    host: &str,
) -> Result<ServerCertVerified, rustls::Error> {
    verifier.verify_server_cert(
        cert.der(),
        &[],
        &ServerName::try_from(host).unwrap(),
        &[],
        UnixTime::since_unix_epoch(Duration::from_secs(1_800_000_000)),
    )
}

#[test]
fn oeffentliche_pruefung_akzeptiert_erneuerung_aber_keinen_falschen_server_oder_zeitraum() {
    let (ca, issuer) = authority();
    let old = certificate(&issuer, "ftp.example.test", 2020, 2030);
    let renewed = certificate(&issuer, "ftp.example.test", 2026, 2031);
    assert_ne!(old.der(), renewed.der());
    let verifier = with_authority(PinnedCertificateVerifier::public().unwrap(), &ca);
    verify(&verifier, &old, "ftp.example.test").unwrap();
    verify(&verifier, &renewed, "ftp.example.test").unwrap();
    assert_eq!(
        *lock(&verifier.observed),
        Some(Sha256::digest(renewed.der().as_ref()).into())
    );
    assert!(verify(&verifier, &renewed, "anderer.example.test").is_err());
    assert!(verify(
        &verifier,
        &certificate(&issuer, "ftp.example.test", 2020, 2021),
        "ftp.example.test"
    )
    .is_err());
    assert!(verify(
        &verifier,
        &certificate(&issuer, "ftp.example.test", 2035, 2040),
        "ftp.example.test"
    )
    .is_err());
    let (_, unknown_issuer) = authority();
    assert!(verify(
        &verifier,
        &certificate(&unknown_issuer, "ftp.example.test", 2020, 2030),
        "ftp.example.test"
    )
    .is_err());
    assert!(verifier.public_identity_rejected());
}

#[test]
fn ausdrueckliche_pin_modi_behalten_ihre_exakte_bindung() {
    let (ca, issuer) = authority();
    let old = certificate(&issuer, "ftp.example.test", 2020, 2030);
    let renewed = certificate(&issuer, "ftp.example.test", 2026, 2031);
    let pin = Sha256::digest(old.der().as_ref()).into();
    for policy in [
        FtpsCertificatePolicy::WebPkiAndPin,
        FtpsCertificatePolicy::PinnedCertificate,
        FtpsCertificatePolicy::UnverifiedCertificate,
    ] {
        let mut verifier = PinnedCertificateVerifier::new(pin, policy).unwrap();
        if policy == FtpsCertificatePolicy::WebPkiAndPin {
            verifier = with_authority(verifier, &ca);
        }
        verify(&verifier, &old, "ftp.example.test").unwrap();
        assert!(
            verify(&verifier, &renewed, "ftp.example.test").is_err(),
            "{policy:?}"
        );
    }
}
