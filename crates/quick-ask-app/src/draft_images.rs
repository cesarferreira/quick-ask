use crate::attachments::MAX_ATTACHMENTS;
use anyhow::Result;
use quick_ask_core::chat::ImageAttachment;
use std::path::Path;

#[derive(Default)]
pub struct DraftImages {
    pub images: Vec<ImageAttachment>,
    pub loading: bool,
    epoch: u64,
}

impl DraftImages {
    pub fn begin_import(&mut self, count: usize) -> Result<u64> {
        anyhow::ensure!(
            !self.loading,
            "Wait for the current images to finish preparing."
        );
        anyhow::ensure!(count > 0, "Choose an image to attach.");
        anyhow::ensure!(
            count <= MAX_ATTACHMENTS.saturating_sub(self.images.len()),
            "Attach up to four images per question. Remove an image to add another."
        );
        self.epoch = self.epoch.wrapping_add(1);
        self.loading = true;
        Ok(self.epoch)
    }

    pub fn complete(&mut self, epoch: u64, images: Vec<ImageAttachment>) -> bool {
        if epoch != self.epoch || !self.loading {
            return false;
        }
        self.loading = false;
        for image in images {
            if self.images.len() < MAX_ATTACHMENTS
                && !self
                    .images
                    .iter()
                    .any(|existing| existing.path == image.path)
            {
                self.images.push(image);
            }
        }
        true
    }

    pub fn remove(&mut self, path: &Path) {
        self.images.retain(|image| image.path != path);
    }

    pub fn clear(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.loading = false;
        self.images.clear();
    }

    pub fn take(&mut self) -> Vec<ImageAttachment> {
        let images = std::mem::take(&mut self.images);
        self.clear();
        images
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(name: &str) -> ImageAttachment {
        ImageAttachment {
            name: name.into(),
            path: format!("/images/{name}.png").into(),
        }
    }

    #[test]
    fn clearing_the_draft_ignores_images_still_loading_for_the_previous_chat() {
        let mut draft = DraftImages::default();
        let old = draft.begin_import(1).unwrap();
        draft.clear();
        let current = draft.begin_import(1).unwrap();
        assert!(!draft.complete(old, vec![image("private-old")]));
        assert!(draft.loading);
        assert!(draft.complete(current, vec![image("current")]));
        assert_eq!(draft.take(), vec![image("current")]);
        assert!(draft.images.is_empty());
    }

    #[test]
    fn capacity_includes_current_images_and_an_import_in_progress() {
        let mut draft = DraftImages::default();
        let ticket = draft.begin_import(3).unwrap();
        assert!(draft.begin_import(1).is_err());
        assert!(draft.complete(ticket, vec![image("one"), image("two"), image("three")]));
        assert!(draft.begin_import(2).is_err());
        draft.remove(Path::new("/images/two.png"));
        let ticket = draft.begin_import(2).unwrap();
        assert!(draft.complete(ticket, vec![image("four"), image("five")]));
        assert_eq!(draft.images.len(), 4);
    }

    #[test]
    fn duplicate_image_imports_keep_one_thumbnail_and_removal_keeps_other_images() {
        let mut draft = DraftImages::default();
        let ticket = draft.begin_import(3).unwrap();
        assert!(draft.complete(ticket, vec![image("one"), image("one"), image("two")]));
        assert_eq!(draft.images.len(), 2);
        draft.remove(Path::new("/images/one.png"));
        assert_eq!(draft.images, vec![image("two")]);
        draft.clear();
        assert!(!draft.loading);
        assert!(draft.images.is_empty());
    }
}
