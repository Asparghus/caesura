use colored::Colorize;
use di::{injectable, Ref, RefMut};
use log::*;

use crate::api::Api;
use crate::errors::AppError;
use crate::formats::TargetFormatProvider;
use crate::fs::{Collector, PathManager};
use crate::imdl::imdl_command::ImdlCommand;
use crate::naming::Shortener;
use crate::options::verify_options::VerifyOptions;
use crate::options::Options;
use crate::source::*;
use crate::verify::tag_verifier::TagVerifier;
use crate::verify::SourceRule::*;
use crate::verify::*;

/// Verify a FLAC source is suitable for transcoding.
#[injectable]
pub struct VerifyCommand {
    options: Ref<VerifyOptions>,
    api: RefMut<Api>,
    targets: Ref<TargetFormatProvider>,
    paths: Ref<PathManager>,
}

impl VerifyCommand {
    pub async fn execute(&mut self, source: &Source) -> Result<bool, AppError> {
        info!("{} {}", "Verifying".bold(), source);
        let api_errors = self.api_checks(source);
        debug_errors(&api_errors, source, "API checks");
        let flac_errors = self.flac_checks(source)?;
        debug_errors(&flac_errors, source, "FLAC file checks");
        let hash_check = if self.options.get_value(|x| x.skip_hash_check) {
            debug!("{} hash check due to settings", "Skipped".bold());
            Vec::new()
        } else {
            let hash_check = self.hash_check(source).await?;
            debug_errors(&hash_check, source, "Hash check");
            hash_check
        };
        let is_verified = api_errors.is_empty() && flac_errors.is_empty() && hash_check.is_empty();
        if is_verified {
            info!("{} {}", "Verified".bold(), source);
        } else {
            warn!("{} {}", "Skipped".bold().yellow(), source);
            warn_errors(api_errors);
            warn_errors(flac_errors);
            warn_errors(hash_check);
        }
        Ok(is_verified)
    }

    fn api_checks(&self, source: &Source) -> Vec<SourceRule> {
        let mut errors: Vec<SourceRule> = Vec::new();
        if source.torrent.scene {
            errors.push(SceneNotSupported);
        }
        if source.torrent.lossy_master_approved == Some(true) {
            errors.push(LossyMasterNeedsApproval);
        }
        if source.torrent.lossy_web_approved == Some(true) {
            errors.push(LossyWebNeedsApproval);
        }
        let target_formats = self.targets.get(source.format, &source.existing);
        if target_formats.is_empty() {
            errors.push(NoTranscodeFormats);
        }
        errors
    }

    fn flac_checks(&self, source: &Source) -> Result<Vec<SourceRule>, AppError> {
        if !source.directory.exists() || !source.directory.is_dir() {
            return Ok(vec![SourceDirectoryNotFound(
                source.directory.to_string_lossy().to_string(),
            )]);
        }
        let flacs = Collector::get_flacs(&source.directory);
        if flacs.is_empty() {
            return Ok(vec![NoFlacFiles(
                source.directory.to_string_lossy().to_string(),
            )]);
        }
        let mut errors: Vec<SourceRule> = Vec::new();
        for flac in flacs {
            let max_path = self.paths.get_max_transcode_sub_path(source, &flac);
            if max_path.len() > MAX_PATH_LENGTH {
                errors.push(PathTooLong(max_path));
                Shortener::suggest_track_name(&flac);
            }
            for error in TagVerifier::execute(&flac, &source.metadata.media)? {
                errors.push(error);
            }
            for error in StreamVerifier::execute(&flac)? {
                errors.push(error);
            }
        }
        if errors.iter().any(|rule| matches!(rule, &PathTooLong(_))) {
            Shortener::suggest_album_name(source);
        }
        Ok(errors)
    }

    async fn hash_check(&mut self, source: &Source) -> Result<Vec<SourceRule>, AppError> {
        let mut api = self.api.write().expect("API should be available");
        let buffer = api.get_torrent_file_as_buffer(source.torrent.id).await?;
        ImdlCommand::verify_from_buffer(&buffer, &source.directory).await
    }
}

fn debug_errors(errors: &Vec<SourceRule>, source: &Source, title: &str) {
    if errors.is_empty() {
        debug!("{} {} {}", "Passed".bold(), title, source);
    } else {
        debug!("{} {} {}", "Failed".bold().red(), title, source);
        for error in errors {
            debug!("{} {}", "⚠".yellow(), error);
        }
    }
}

fn warn_errors(errors: Vec<SourceRule>) {
    for error in errors {
        warn!("{}", error);
    }
}
