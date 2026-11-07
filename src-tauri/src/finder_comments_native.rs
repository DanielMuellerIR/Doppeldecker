//! Native Finder-Kommentare über typisierte AppleEvents, ohne Scriptinterpolation.
use doppeldecker_core::file_properties::{
    validate_finder_comment, FinderCommentAccess, FinderCommentWriteError, MAX_FINDER_COMMENT_BYTES,
};
use objc2::rc::{autoreleasepool, Retained};
use objc2::AllocAnyThread;
use objc2_core_services::{AEDesc, CreateObjSpecifier};
use objc2_foundation::{
    NSAppleEventDescriptor as Descriptor, NSAppleEventSendOptions, NSString, NSURL,
};
use std::{
    ffi::c_void,
    io,
    os::fd::{AsRawFd, BorrowedFd},
};
use std::{mem::MaybeUninit, ptr::NonNull};

const CORE: u32 = u32::from_be_bytes(*b"core");
const GET: u32 = u32::from_be_bytes(*b"getd");
const SET: u32 = u32::from_be_bytes(*b"setd");
const DIRECT: u32 = u32::from_be_bytes(*b"----");
#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn AEDeterminePermissionToAutomateTarget(
        target: *const c_void,
        event_class: u32,
        event_id: u32,
        ask_user: u8,
    ) -> i32;
}

pub(crate) struct NativeFinderComments {
    request_permission: bool,
    permission_checked: bool,
}
impl NativeFinderComments {
    pub(crate) fn new(request_permission: bool) -> Self {
        Self {
            request_permission,
            permission_checked: false,
        }
    }
    fn permission(&mut self) -> io::Result<()> {
        if self.permission_checked {
            return Ok(());
        }
        let target =
            Descriptor::descriptorWithBundleIdentifier(&NSString::from_str("com.apple.finder"));
        // Ohne ausdrückliche Schaltfläche wird nie ein Systemdialog angefordert.
        let status = unsafe {
            AEDeterminePermissionToAutomateTarget(
                target.aeDesc().cast(),
                CORE,
                GET,
                u8::from(self.request_permission),
            )
        };
        match status {
            0 => { self.permission_checked = true; Ok(()) }
            -1744 => Err(doppeldecker_core::file_properties::finder_comment_permission_required()),
            -1743 => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Finder-Zugriff wurde in macOS abgelehnt. Die Freigabe lässt sich unter Datenschutz & Sicherheit → Automation prüfen.",
            )),
            code => Err(io::Error::other(format!(
                "Finder-Zugriff konnte nicht vorbereitet werden (macOS {code})"
            ))),
        }
    }
}
fn comment_specifier(object: BorrowedFd<'_>) -> io::Result<Retained<Descriptor>> {
    property_specifier(object, u32::from_be_bytes(*b"comt"))
}
fn property_specifier(
    object: BorrowedFd<'_>,
    property_id: u32,
) -> io::Result<Retained<Descriptor>> {
    // Der FD bleibt beim Core. /dev/fd bezeichnet exakt dieses geöffnete Objekt;
    // erst die daraus gebildete Objekt-ID geht an Finder, niemals der FD-Pfad.
    let descriptor_path = NSString::from_str(&format!("/dev/fd/{}", object.as_raw_fd()));
    let url = NSURL::fileURLWithPath(&descriptor_path);
    let reference = url
        .fileReferenceURL()
        .filter(|url| url.isFileReferenceURL())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "Keine native Dateireferenz für diesen Kommentarzugriff; kein Pfad-Fallback",
            )
        })?;
    let file_reference = Descriptor::descriptorWithFileURL(&reference);
    // Finder verlangt als Container einen Objektspezifizierer. Die gebundene
    // Dateireferenz ist dessen Selektor, nicht selbst der Property-Container.
    let item = build_object(
        u32::from_be_bytes(*b"cobj"),
        &Descriptor::nullDescriptor(),
        u32::from_be_bytes(*b"name"),
        &file_reference,
    )?;
    build_property(&item, property_id)
}
fn build_property(
    object_reference: &Descriptor,
    property_id: u32,
) -> io::Result<Retained<Descriptor>> {
    build_object(
        u32::from_be_bytes(*b"prop"),
        object_reference,
        u32::from_be_bytes(*b"prop"),
        &Descriptor::descriptorWithTypeCode(property_id),
    )
}
fn build_object(
    wanted: u32,
    container: &Descriptor,
    form: u32,
    selector: &Descriptor,
) -> io::Result<Retained<Descriptor>> {
    let mut result = MaybeUninit::<AEDesc>::uninit();
    // Die öffentliche Kodierungsfunktion übernimmt hier keine Eingabedaten.
    // Erst der fertige Deskriptor wechselt in den Besitz von Cocoa.
    let status = unsafe {
        CreateObjSpecifier(
            wanted,
            container.aeDesc().cast_mut(),
            form,
            selector.aeDesc().cast_mut(),
            false,
            result.as_mut_ptr(),
        )
    };
    if status != 0 {
        return Err(io::Error::other(format!(
            "Native Objektkodierung fehlgeschlagen (macOS {status})"
        )));
    }
    let mut result = unsafe { result.assume_init() };
    // initWithAEDescNoCopy übernimmt den nativen Handle. result darf deshalb
    // nicht noch einmal mit AEDisposeDesc freigegeben werden.
    Ok(
        unsafe {
            Descriptor::initWithAEDescNoCopy(Descriptor::alloc(), NonNull::from(&mut result))
        },
    )
}
fn event(kind: u32, property: &Descriptor, text: Option<&str>) -> Retained<Descriptor> {
    let target =
        Descriptor::descriptorWithBundleIdentifier(&NSString::from_str("com.apple.finder"));
    let event =
        Descriptor::appleEventWithEventClass_eventID_targetDescriptor_returnID_transactionID(
            CORE,
            kind,
            Some(&target),
            -1,
            0,
        );
    event.setParamDescriptor_forKeyword(property, DIRECT);
    if let Some(text) = text {
        event.setParamDescriptor_forKeyword(
            &Descriptor::descriptorWithString(&NSString::from_str(text)),
            u32::from_be_bytes(*b"data"),
        );
    }
    event
}
fn send(event: &Descriptor) -> io::Result<Retained<Descriptor>> {
    let options = NSAppleEventSendOptions::WaitForReply
        | NSAppleEventSendOptions::NeverInteract
        | NSAppleEventSendOptions::DontRecord;
    let reply = event
        .sendEventWithOptions_timeout_error(options, 5.0)
        .map_err(|error| {
            native_error_detail(
                error.code() as i32,
                Some(error.localizedDescription().to_string()),
            )
        })?;
    if let Some(error) = reply.paramDescriptorForKeyword(u32::from_be_bytes(*b"errn")) {
        if error.int32Value() != 0 {
            let detail = reply
                .paramDescriptorForKeyword(u32::from_be_bytes(*b"errs"))
                .and_then(|value| value.stringValue())
                .map(|value| value.to_string());
            return Err(native_error_detail(error.int32Value(), detail));
        }
    }
    Ok(reply)
}
fn native_error_detail(code: i32, detail: Option<String>) -> io::Error {
    let error = native_error(code);
    if let Some(detail) = detail.filter(|value| !value.is_empty()) {
        return io::Error::new(
            error.kind(),
            format!("{error}: {}", detail.chars().take(2048).collect::<String>()),
        );
    }
    error
}
fn native_error(code: i32) -> io::Error {
    let kind = match code {
        -1743 | -1744 => io::ErrorKind::PermissionDenied,
        -43 | -1728 => io::ErrorKind::NotFound,
        -1712 => io::ErrorKind::TimedOut,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(
        kind,
        format!("Finder-Kommentarzugriff fehlgeschlagen (macOS {code})"),
    )
}
impl FinderCommentAccess for NativeFinderComments {
    fn read(&mut self, object: BorrowedFd<'_>) -> io::Result<String> {
        autoreleasepool(|_| {
            self.permission()?;
            let property = comment_specifier(object)?;
            let reply = send(&event(GET, &property, None))?;
            let value = reply
                .paramDescriptorForKeyword(DIRECT)
                .and_then(|value| value.stringValue())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Finder lieferte keinen Kommentartext",
                    )
                })?;
            if value.length() > MAX_FINDER_COMMENT_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Finder-Kommentar überschreitet die Textgrenze",
                ));
            }
            let text = value.to_string();
            validate_finder_comment(&text).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Finder-Kommentar überschreitet die Textgrenze",
                )
            })?;
            Ok(text)
        })
    }
    fn write(&mut self, object: BorrowedFd<'_>, text: &str) -> Result<(), FinderCommentWriteError> {
        autoreleasepool(|_| {
            validate_finder_comment(text).map_err(FinderCommentWriteError::NotStarted)?;
            self.permission()
                .map_err(FinderCommentWriteError::NotStarted)?;
            let property =
                comment_specifier(object).map_err(FinderCommentWriteError::NotStarted)?;
            // Ab dem Senden kann auch eine fehlende Antwort eine echte Teilwirkung
            // bedeuten. Der Core stoppt dann die Auswahl und meldet den Pfad.
            send(&event(SET, &property, Some(text)))
                .map(|_| ())
                .map_err(FinderCommentWriteError::OutcomeUnclear)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::{fd::AsFd, unix::fs::MetadataExt},
    };
    #[test]
    fn native_comment_reference_keeps_the_held_object_after_rename_and_path_substitution() {
        autoreleasepool(|_| {
            let area = tempfile::tempdir().unwrap();
            for is_dir in [false, true] {
                let path = area.path().join(if is_dir { "folder" } else { "file" });
                if is_dir {
                    fs::create_dir(&path).unwrap();
                } else {
                    fs::write(&path, b"original").unwrap();
                }
                let object = fs::File::open(&path).unwrap();
                let before = object.metadata().unwrap();
                let property = comment_specifier(object.as_fd()).unwrap();
                let container = property
                    .descriptorForKeyword(u32::from_be_bytes(*b"from"))
                    .unwrap();
                assert_eq!(container.descriptorType(), u32::from_be_bytes(*b"obj "));
                let file_reference = container
                    .descriptorForKeyword(u32::from_be_bytes(*b"seld"))
                    .unwrap();
                assert_eq!(
                    file_reference.descriptorType(),
                    u32::from_be_bytes(*b"furl")
                );
                let moved = path.with_extension("moved");
                fs::rename(&path, &moved).unwrap();
                fs::write(&path, b"substitute").unwrap();
                let reference = file_reference.fileURLValue().unwrap();
                let resolved = reference.filePathURL().unwrap().path().unwrap().to_string();
                let actual = fs::metadata(&resolved).unwrap();
                assert_eq!((actual.dev(), actual.ino()), (before.dev(), before.ino()));
                assert_eq!(
                    fs::canonicalize(resolved).unwrap(),
                    fs::canonicalize(moved).unwrap()
                );
            }
        });
    }
    #[test]
    fn native_comment_payload_is_text_and_never_interpolated_as_a_script() {
        autoreleasepool(|_| {
            let area = tempfile::tempdir().unwrap();
            let path = area.path().join("quotes.txt");
            fs::write(&path, b"data").unwrap();
            let file = fs::File::open(path).unwrap();
            let property = comment_specifier(file.as_fd()).unwrap();
            let text = "ä \" ; tell application \"Finder\" to delete everything\n<&>";
            let request = event(SET, &property, Some(text));
            assert_eq!(request.eventClass(), CORE);
            assert_eq!(request.eventID(), SET);
            assert_eq!(
                request
                    .paramDescriptorForKeyword(u32::from_be_bytes(*b"data"))
                    .unwrap()
                    .stringValue()
                    .unwrap()
                    .to_string(),
                text
            );
        });
    }
}
