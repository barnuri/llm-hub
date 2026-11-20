//! The text layer of a PDF. Built in only with the `media-pdf` cargo feature, which adds a PDF
//! parser to the binary. Without it every PDF becomes a placeholder.

/// The text of every page of `pdf`, in page order.
///
/// # Errors
/// A PDF that does not parse, or one with no text layer, such as a scan.
#[cfg(feature = "media-pdf")]
pub(crate) fn pdf_text(pdf: &[u8]) -> Result<String, String> {
    let document = lopdf::Document::load_mem(pdf).map_err(|e| format!("bad PDF: {e}"))?;
    let pages: Vec<u32> = document.get_pages().keys().copied().collect();
    let text = document
        .extract_text(&pages)
        .map_err(|e| format!("no text: {e}"))?;
    let text = text.trim();
    if text.is_empty() {
        return Err("no text layer (a scanned PDF?)".to_owned());
    }
    Ok(text.to_owned())
}

/// Without the `media-pdf` feature, no PDF has text.
///
/// # Errors
/// Always: the parser is not built in.
#[cfg(not(feature = "media-pdf"))]
pub(crate) fn pdf_text(_pdf: &[u8]) -> Result<String, String> {
    Err("PDF text extraction is not built in (cargo feature media-pdf)".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "media-pdf"))]
    #[test]
    fn without_the_feature_a_pdf_has_no_text() {
        let err = pdf_text(b"%PDF-1.4").unwrap_err();
        assert!(err.contains("media-pdf"), "{err}");
    }

    #[cfg(feature = "media-pdf")]
    #[test]
    fn a_bad_pdf_is_an_error() {
        assert!(pdf_text(b"not a pdf").is_err());
    }

    #[cfg(feature = "media-pdf")]
    #[test]
    fn a_text_pdf_gives_its_text() {
        use lopdf::content::{Content, Operation};
        use lopdf::{Document, Object, Stream, dictionary};

        let mut doc = Document::with_version("1.5");
        let tree_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Courier",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![100.into(), 600.into()]),
                Operation::new("Tj", vec![Object::string_literal("Hello PDF")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => tree_id, "Contents" => content_id,
        });
        doc.objects.insert(
            tree_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
                "Resources" => resources_id, "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => tree_id });
        doc.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();

        assert_eq!(pdf_text(&bytes).unwrap(), "Hello PDF");
    }
}
