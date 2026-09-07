// SPDX-License-Identifier: AGPL-3.0-only
//! Diagnose-Tool: instanziert den registrierten „OBS Virtual Camera“-
//! DirectShow-Filter direkt (CoCreateInstance) und listet Pins + Media-Types
//! (IAMStreamConfig), während ein Writer (spike_gradient) die Queue füttert.
//! Antwortet der Filter mit 0 Formaten, war die Queue beim Filter-Constructor
//! nicht READY; liefert er Formate, liegt das ffmpeg-Problem in dessen
//! Format-Auswahl.
//!
//! Ausführen: cargo run -p chiaki-virtualcam --example probe_filter

use windows::core::{GUID, Interface as _};
use windows::Win32::Media::DirectShow::{
    IBaseFilter, IAMStreamConfig, IEnumPins, IPin, PIN_INFO, PINDIR_OUTPUT,
};
use windows::Win32::Media::KernelStreaming::IKsPropertySet;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

// {A3FCE0F5-3493-419F-958A-ABA1250EC20B}
const OBS_VIRTUAL_CAM: GUID = GUID::from_u128(0xa3fce0f5_3493_419f_958a_aba1250ec20b);
// AMPROPSETID_Pin {CCA8CDEF-83C3-4A75-94A7-9EA4C8EA2B5B}, AMPROPERTY_PIN_CATEGORY
const AMPROPSETID_PIN: GUID = GUID::from_u128(0xcca8cdef_83c3_4a75_94a7_9ea4c8ea2b5b);
const AMPROPERTY_PIN_CATEGORY: u32 = 1;
// PIN_CATEGORY_CAPTURE {FB6C4281-0353-11D1-905F-0000C0CC16BA}
const PIN_CATEGORY_CAPTURE: GUID = GUID::from_u128(0xfb6c4281_0353_11d1_905f_0000c0cc16ba);
// MEDIATYPE_Video {73646976-0000-0010-8000-00AA00389B71}
const MEDIATYPE_VIDEO: GUID = GUID::from_u128(0x73646976_0000_0010_8000_00aa00389b71);

fn main() {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED);
        println!("CoInitializeEx ok.");

        // Sichtbarkeit der Writer-Queue aus DIESEM Prozess (wie der Filter-
        // Constructor: OpenFileMappingW FILE_MAP_READ + Header lesen).
        let name: Vec<u16> = "OBSVirtualCamVideo\0".encode_utf16().collect();
        match windows::Win32::System::Memory::OpenFileMappingW(
            windows::Win32::System::Memory::FILE_MAP_READ.0,
            false,
            windows::core::PCWSTR(name.as_ptr()),
        ) {
            Ok(handle) => {
                let view = windows::Win32::System::Memory::MapViewOfFile(
                    handle,
                    windows::Win32::System::Memory::FILE_MAP_READ,
                    0,
                    0,
                    0,
                );
                if !view.Value.is_null() {
                    let base = view.Value as *const u32;
                    let (write_idx, read_idx, state, cx, cy) = (*base, *base.add(1), *base.add(2), *base.add(7), *base.add(8));
                    println!(
                        "Queue sichtbar: write={write_idx} read={read_idx} state={state} (READY=2) {}x{}",
                        cx, cy
                    );
                    let _ = windows::Win32::System::Memory::UnmapViewOfFile(view);
                } else {
                    println!("Queue-Mapping offen, aber MapViewOfFile fehlgeschlagen");
                }
                let _ = windows::Win32::Foundation::CloseHandle(handle);
            }
            Err(err) => println!("Queue NICHT sichtbar (OpenFileMappingW): {err}"),
        }

        let filter: IBaseFilter =
            CoCreateInstance(&OBS_VIRTUAL_CAM, None, CLSCTX_INPROC_SERVER)
                .expect("OBS-VC-Filter instanziierbar (registriert?)");
        println!("Filter instanziert.");

        let pins: IEnumPins = filter.EnumPins().expect("EnumPins");
        let mut slot = [None::<IPin>; 1];
        let mut index = 0usize;
        while pins.Next(&mut slot, None) == windows::core::HRESULT(0) {
            let Some(pin) = slot[0].take() else { break };
            let mut info = PIN_INFO::default();
            pin.QueryPinInfo(&mut info)
                .expect("QueryPinInfo");
            let dir_output = info.dir == PINDIR_OUTPUT;
            let name = String::from_utf16_lossy(
                &info.achName[..info.achName.iter().position(|c| *c == 0).unwrap_or(0)],
            );

            // Pin-Kategorie (IKsPropertySet).
            let mut cat = GUID::zeroed();
            let mut got = 0u32;
            let cat_ok = pin
                .cast::<IKsPropertySet>()
                .ok()
                .and_then(|ks| {
                    ks.Get(
                        &AMPROPSETID_PIN,
                        AMPROPERTY_PIN_CATEGORY,
                        std::ptr::null(),
                        0,
                        &mut cat as *mut _ as *mut core::ffi::c_void,
                        std::mem::size_of::<GUID>() as u32,
                        &mut got,
                    )
                    .ok()
                })
                .is_some();
            let cat_name = if cat_ok {
                if cat == PIN_CATEGORY_CAPTURE { "CAPTURE" } else { "andere" }
            } else {
                "keine"
            };
            println!("Pin #{index}: „{name}“ dir-output={dir_output} kategorie={cat_name}");

            // Media-Types (IAMStreamConfig — genau der ffmpeg-Pfad).
            match pin.cast::<IAMStreamConfig>() {
                Ok(config) => {
                    let (mut count, mut size) = (0i32, 0i32);
                    match config.GetNumberOfCapabilities(&mut count, &mut size) {
                        Ok(()) => {
                            println!("  Formate (GetNumberOfCapabilities): {count}");
                            let mut caps = vec![0u8; size.max(0) as usize];
                            for i in 0..count {
                                let mut mt_ptr: *mut windows::Win32::Media::MediaFoundation::AM_MEDIA_TYPE =
                                    std::ptr::null_mut();
                                match config.GetStreamCaps(i, &mut mt_ptr, caps.as_mut_ptr()) {
                                    Ok(()) => {
                                        let mt = &*mt_ptr;
                                        let (wh, interval) = if mt.formattype
                                            == windows::Win32::Media::MediaFoundation::FORMAT_VideoInfo
                                            && !mt.pbFormat.is_null()
                                        {
                                            let vih = &*(mt.pbFormat
                                                as *const windows::Win32::Media::MediaFoundation::VIDEOINFOHEADER);
                                            (
                                                (vih.bmiHeader.biWidth, vih.bmiHeader.biHeight),
                                                vih.AvgTimePerFrame,
                                            )
                                        } else {
                                            ((0, 0), 0)
                                        };
                                        println!(
                                            "    [{i}] major=video? {} {}x{} interval {interval}",
                                            mt.majortype == MEDIATYPE_VIDEO,
                                            wh.0,
                                            wh.1,
                                        );
                                        windows::Win32::System::Com::CoTaskMemFree(
                                            Some(mt_ptr.cast()),
                                        );
                                    }
                                    Err(err) => println!("    [{i}] GetStreamCaps: {err}"),
                                }
                            }
                        }
                        Err(err) => println!("  GetNumberOfCapabilities fehlgeschlagen: {err}"),
                    }
                    // GetFormat — der Default-Format-Pfad von ffmpeg.
                    match config.GetFormat() {
                        Ok(mt) => {
                            println!("  GetFormat: OK");
                            windows::Win32::System::Com::CoTaskMemFree(Some(mt.cast()));
                        }
                        Err(err) => println!("  GetFormat fehlgeschlagen: {err}"),
                    }
                }
                Err(_) => println!("  Pin unterstützt KEIN IAMStreamConfig!"),
            }
            index += 1;
        }
        if index == 0 {
            println!("KEINE Pins am Filter!");
        }
    }
}
