use super::*;

use std::io::{BufReader, BufWriter};

use util::Error;

#[test]
fn test_extension_use_srtp() -> Result<(), Error> {
    let raw_use_srtp = vec![0x00, 0x05, 0x00, 0x02, 0x00, 0x01, 0x00]; //0x00, 0x0e,
    let parsed_use_srtp = ExtensionUseSRTP {
        protection_profiles: vec![SRTPProtectionProfile::SRTP_AES128_CM_HMAC_SHA1_80],
    };

    let mut raw = vec![];
    {
        let mut writer = BufWriter::<&mut Vec<u8>>::new(raw.as_mut());
        parsed_use_srtp.marshal(&mut writer)?;
    }

    assert_eq!(
        raw, raw_use_srtp,
        "extensionUseSRTP marshal: got {:?}, want {:?}",
        raw, raw_use_srtp
    );

    let mut reader = BufReader::new(raw.as_slice());
    let new_use_srtp = ExtensionUseSRTP::unmarshal(&mut reader)?;

    assert_eq!(
        new_use_srtp, parsed_use_srtp,
        "extensionUseSRTP unmarshal: got {:?}, want {:?}",
        new_use_srtp, parsed_use_srtp
    );

    Ok(())
}