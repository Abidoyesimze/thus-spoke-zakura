use super::*;

#[test]
fn project_images_are_version_locked() {
    assert_eq!(
        app_image(),
        format!(
            "ghcr.io/zcashlabs/thus-spoke-zakura-app:{}",
            env!("CARGO_PKG_VERSION")
        )
    );
    assert_eq!(
        lightwalletd_image(),
        format!(
            "ghcr.io/zcashlabs/thus-spoke-zakura-lightwalletd:{}",
            env!("CARGO_PKG_VERSION")
        )
    );
}
