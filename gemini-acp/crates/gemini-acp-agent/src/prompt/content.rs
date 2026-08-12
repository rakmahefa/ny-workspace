//! Conversion des `ContentBlock` ACP en texte + images extraites.
//!
//! Responsabilité unique : transformer les blocs de contenu du protocole ACP
//! en (texte, images) pour le prompt Gemini. Les images sont extraites en
//! paires `(base64, mime)` pour l'upload Scotty (spec §4.2).

use agent_client_protocol::schema::v1::{ContentBlock, EmbeddedResourceResource};

/// Convertit les `ContentBlock` du client : texte + ressources en texte, et
/// **images** (`ContentBlock::Image`) extraites en paires `(base64, mime)`
/// pour l'upload Scotty (spec §4.2 — refs dans `inner[0][3]`).
pub fn blocks_to_parts(blocks: &[ContentBlock]) -> (String, Vec<(String, String)>) {
    let mut text = String::new();
    let mut images = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => text.push_str(&t.text),
            ContentBlock::Image(img) => {
                images.push((img.data.clone(), img.mime_type.clone()));
                text.push_str("\n[image jointe — analyse et décris son contenu]\n");
            }
            ContentBlock::ResourceLink(l) => text.push_str(&format!(
                "{}\n\n[ressource référencée — contenu non inclus, l'utilisateur la parcourt lui-même]",
                l.uri
            )),
            ContentBlock::Resource(r) => match &r.resource {
                EmbeddedResourceResource::TextResourceContents(t) => text.push_str(&t.text),
                EmbeddedResourceResource::BlobResourceContents(_) => {
                    text.push_str("[ressource binaire non prise en charge en v1]")
                }
                // Schéma non-exhaustif.
                _ => text.push_str("[ressource non prise en charge en v1]"),
            },
            ContentBlock::Audio(_) => {
                text.push_str("[audio non pris en charge en v1]")
            }
            // Schéma non-exhaustif : toute future variante → note générique.
            _ => text.push_str("[bloc de contenu non pris en charge en v1]"),
        }
        text.push('\n');
    }
    (text.trim_end().to_string(), images)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{ImageContent, ResourceLink, TextContent};

    #[test]
    fn blocs_vers_texte_et_images() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("bonjour")),
            ContentBlock::ResourceLink(ResourceLink::new("fichier", "file:///etc/hosts")),
            ContentBlock::Image(ImageContent::new("aGVsbG8=", "image/png")),
        ];
        let (text, images) = blocks_to_parts(&blocks);
        assert!(text.contains("bonjour"));
        assert!(text.contains("file:///etc/hosts"));
        assert!(text.contains("image jointe"));
        assert_eq!(
            images,
            vec![("aGVsbG8=".to_string(), "image/png".to_string())]
        );
    }
}