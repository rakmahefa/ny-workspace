//! Conversion des `ContentBlock` ACP en texte + images extraites.

use agent_client_protocol::schema::v1::{ContentBlock, EmbeddedResourceResource};

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
                EmbeddedResourceResource::BlobResourceContents(_) => text.push_str("[ressource binaire non prise en charge en v1]"),
                _ => text.push_str("[ressource non prise en charge en v1]"),
            },
            ContentBlock::Audio(_) => text.push_str("[audio non pris en charge en v1]"),
            _ => text.push_str("[bloc de contenu non pris en charge en v1]"),
        }
        text.push('\n');
    }
    (text.trim_end().to_string(), images)
}

#[cfg(test)]
#[path = "../test/content.rs"]
mod tests;
