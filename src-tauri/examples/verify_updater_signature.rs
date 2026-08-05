use minisign_verify::{PublicKey, Signature};
use std::{env, fs::File, io::Read, path::Path};

fn verify(installer: &Path, signature: &Path, public_key: &Path) -> Result<(), String> {
    let key = PublicKey::from_file(public_key)
        .map_err(|error| format!("could not read updater public key: {error}"))?;
    let signature = Signature::from_file(signature)
        .map_err(|error| format!("could not read updater signature: {error}"))?;
    let mut verifier = key
        .verify_stream(&signature)
        .map_err(|error| format!("could not initialize updater verification: {error}"))?;
    let mut file = File::open(installer)
        .map_err(|error| format!("could not open installer for verification: {error}"))?;
    // Heap, not stack: a 1 MiB stack array overflows the Windows main
    // thread's default 1 MB stack in a debug build — which is exactly how
    // the release workflow runs this example, and how the first-ever signed
    // release run died with STATUS_STACK_OVERFLOW after an otherwise
    // successful build.
    let mut chunk = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut chunk)
            .map_err(|error| format!("could not read installer for verification: {error}"))?;
        if read == 0 {
            break;
        }
        verifier.update(&chunk[..read]);
    }
    verifier.finalize().map_err(|error| {
        format!("updater signature does not match the installer and embedded key: {error}")
    })
}

fn main() -> Result<(), String> {
    let mut args = env::args_os().skip(1);
    let installer = args.next().ok_or_else(|| {
        "usage: verify_updater_signature <installer> <signature> <public-key>".to_string()
    })?;
    let signature = args.next().ok_or_else(|| {
        "usage: verify_updater_signature <installer> <signature> <public-key>".to_string()
    })?;
    let public_key = args.next().ok_or_else(|| {
        "usage: verify_updater_signature <installer> <signature> <public-key>".to_string()
    })?;
    if args.next().is_some() {
        return Err(
            "usage: verify_updater_signature <installer> <signature> <public-key>".to_string(),
        );
    }
    verify(
        Path::new(&installer),
        Path::new(&signature),
        Path::new(&public_key),
    )?;
    println!("Updater signature matches the installer and embedded public key.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::{self, write},
        time::{SystemTime, UNIX_EPOCH},
    };

    // Public test vector from minisign-verify's documented example.
    const PUBLIC_KEY: &str = concat!(
        "untrusted comment: minisign public key\n",
        "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3\n",
    );
    const WRONG_PUBLIC_KEY: &str = concat!(
        "untrusted comment: different minisign public key\n",
        "RWQf6LRCGA9i53mlYedO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3\n",
    );
    const SIGNATURE: &str = concat!(
        "untrusted comment: signature from minisign secret key\n",
        "RUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/",
        "z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\n",
        "trusted comment: timestamp:1633700835\tfile:test\tprehashed\n",
        "wLMDjy9FLAuxZ3q4NlEvkgtyhrr0gtTu6KC4KBJdITbbOeAi1zBIYo0v4iTgt8jJpIidRJnp94ABQkJAgAooBQ==\n",
    );

    fn temp_dir() -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is before the Unix epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!("backlog-updater-verify-{unique}"));
        fs::create_dir(&dir).expect("create test directory");
        dir
    }

    #[test]
    fn valid_signature_passes_and_modified_installer_fails() {
        let dir = temp_dir();
        let installer = dir.join("test.bin");
        let signature = dir.join("test.bin.sig");
        let public_key = dir.join("minisign.pub");
        write(&installer, b"test").expect("write installer");
        write(&signature, SIGNATURE).expect("write signature");
        write(&public_key, PUBLIC_KEY).expect("write public key");

        verify(&installer, &signature, &public_key).expect("documented signature should verify");
        write(&public_key, WRONG_PUBLIC_KEY).expect("write different public key");
        assert!(verify(&installer, &signature, &public_key).is_err());
        write(&public_key, PUBLIC_KEY).expect("restore public key");
        write(&installer, b"tampered").expect("modify installer");
        assert!(verify(&installer, &signature, &public_key).is_err());

        fs::remove_dir_all(dir).expect("remove test directory");
    }
}
