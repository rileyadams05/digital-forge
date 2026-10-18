mod adapter;
pub mod chd;
pub mod n64;
pub mod nintendo_gc_wii;
pub mod ps1;
pub mod ps2;
pub mod ps3;
pub mod ps4;
pub mod ps5;
pub mod psp;
pub mod ufs2tool;
pub mod vita;
pub mod xbox360;
pub mod xbox_orig;

use crate::forge::backend::ConversionBackend;
use crate::forge::platform::Platform;

use adapter::ToolBackend;
pub use adapter::{
    extract_ps3_pkg_for_workflow, find_liborbis_pkg_tool, find_orbis_pkg_tool, find_orbis_pub_cmd,
};

pub fn get_backend_for_id(id: &str) -> Option<Box<dyn ConversionBackend>> {
    let (id, platform) = match id {
        "ps1_psxpackager" => ("ps1_psxpackager", Platform::PlayStation1),
        "ps1_chdman" => ("ps1_chdman", Platform::PlayStation1),
        "passthrough_ps1" => ("passthrough_ps1", Platform::PlayStation1),
        "ps2_maxcso" => ("ps2_maxcso", Platform::PlayStation2),
        "ps2_chdman" => ("ps2_chdman", Platform::PlayStation2),
        "ps2_usbextreme" => ("ps2_usbextreme", Platform::PlayStation2),
        "passthrough_ps2" => ("passthrough_ps2", Platform::PlayStation2),
        "ps3_pkg" => ("ps3_pkg", Platform::PlayStation3),
        "ps3_iso" => ("ps3_iso", Platform::PlayStation3),
        "ps4_pkg_merge" => ("ps4_pkg_merge", Platform::PlayStation4),
        "ps4_liborbispkg" => ("ps4_liborbispkg", Platform::PlayStation4),
        "passthrough_ps4" => ("passthrough_ps4", Platform::PlayStation4),
        "validation_ps4" => ("validation_ps4", Platform::PlayStation4),
        "ps5_ufs2tool" => ("ps5_ufs2tool", Platform::PlayStation5),
        "ps5_mkpfs" => ("ps5_mkpfs", Platform::PlayStation5),
        "ps5_multipart" => ("ps5_multipart", Platform::PlayStation5),
        "ps5_raw_split" => ("ps5_raw_split", Platform::PlayStation5),
        "ps5_anyps5" => ("ps5_anyps5", Platform::PlayStation5),
        "passthrough_ps5" => ("passthrough_ps5", Platform::PlayStation5),
        "psp_maxcso" => ("psp_maxcso", Platform::Psp),
        "passthrough_psp" => ("passthrough_psp", Platform::Psp),
        "vita_pkg2zip" => ("vita_pkg2zip", Platform::PlayStationVita),
        "vita_vpk" => ("vita_vpk", Platform::PlayStationVita),
        "xbox_extract_xiso" => ("xbox_extract_xiso", Platform::OriginalXbox),
        "passthrough_xbox" => ("passthrough_xbox", Platform::OriginalXbox),
        "xbox360_iso2god" => ("xbox360_iso2god", Platform::Xbox360),
        "xbox360_god2iso" => ("xbox360_god2iso", Platform::Xbox360),
        "xbox360_extract_xiso" => ("xbox360_extract_xiso", Platform::Xbox360),
        "xbox360_split" => ("xbox360_split", Platform::Xbox360),
        "native_passthrough" => ("native_passthrough", Platform::NintendoEntertainmentSystem),
        "n64_normalize" => ("n64_normalize", Platform::Nintendo64),
        "optical_chdman" => ("optical_chdman", Platform::SegaCD),
        "nintendo_gc_wii" => ("nintendo_gc_wii", Platform::GameCube),
        _ => return None,
    };
    Some(Box::new(ToolBackend::new(id, platform)))
}

pub fn get_default_backend_for_platform(platform: Platform) -> Box<dyn ConversionBackend> {
    let id = match platform {
        Platform::PlayStation1 => "ps1_psxpackager",
        Platform::PlayStation2 => "ps2_maxcso",
        Platform::PlayStation3 => "ps3_pkg",
        Platform::PlayStation4 => "ps4_liborbispkg",
        Platform::PlayStation5 => "ps5_mkpfs",
        Platform::Psp => "psp_maxcso",
        Platform::PlayStationVita => "vita_pkg2zip",
        Platform::OriginalXbox => "xbox_extract_xiso",
        Platform::Xbox360 => "xbox360_iso2god",
        Platform::Nintendo64 => "n64_normalize",
        Platform::GameCube | Platform::Wii => "nintendo_gc_wii",
        Platform::SegaCD
        | Platform::SegaSaturn
        | Platform::SegaDreamcast
        | Platform::PcEngineCD
        | Platform::PcFx
        | Platform::NeoGeoCD
        | Platform::ThreeDO
        | Platform::PhilipsCdI => "optical_chdman",
        _ => "native_passthrough",
    };
    get_backend_for_id(id).expect("default backend is registered")
}
