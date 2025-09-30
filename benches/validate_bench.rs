use criterion::{black_box, criterion_group, criterion_main, Criterion};
use plexify::commands::validate::ValidateCommand;
use std::fs;
use tempfile::TempDir;
use tokio::runtime::Runtime;

fn create_test_media_library(size: usize) -> TempDir {
    let temp_dir = TempDir::new().unwrap();
    let media_root = temp_dir.path();

    // Create a mix of correctly and incorrectly named files
    for i in 0..size {
        let show_path = media_root.join(format!("Series/Test Show {}/Season 01", i % 10));
        fs::create_dir_all(&show_path).unwrap();

        // Create correctly named files (70%)
        if i % 10 < 7 {
            fs::write(
                show_path.join(format!(
                    "Test Show {} - s01e{:02} - Episode {}.mkv",
                    i % 10,
                    (i % 24) + 1,
                    i
                )),
                "",
            )
            .unwrap();
        } else {
            // Create incorrectly named files (30%)
            fs::write(show_path.join(format!("episode_{}.mkv", i)), "").unwrap();
        }

        // Add some movies too
        if i % 20 == 0 {
            let movie_path = media_root.join(format!(
                "Movies/Test Movie {} ({})",
                i / 20,
                2000 + (i / 20)
            ));
            fs::create_dir_all(&movie_path).unwrap();
            fs::write(
                movie_path.join(format!("Test Movie {} ({}).mkv", i / 20, 2000 + (i / 20))),
                "",
            )
            .unwrap();
        }
    }

    temp_dir
}

fn bench_validate_small(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    c.bench_function("validate_50_files", |b| {
        b.iter(|| {
            let temp_dir = create_test_media_library(50);
            let validate_cmd = ValidateCommand::new(temp_dir.path().to_path_buf());
            let report = rt.block_on(validate_cmd.execute()).unwrap();
            black_box(report);
        });
    });
}

fn bench_validate_medium(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    c.bench_function("validate_200_files", |b| {
        b.iter(|| {
            let temp_dir = create_test_media_library(200);
            let validate_cmd = ValidateCommand::new(temp_dir.path().to_path_buf());
            let report = rt.block_on(validate_cmd.execute()).unwrap();
            black_box(report);
        });
    });
}

fn bench_validate_large(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    c.bench_function("validate_500_files", |b| {
        b.iter(|| {
            let temp_dir = create_test_media_library(500);
            let validate_cmd = ValidateCommand::new(temp_dir.path().to_path_buf());
            let report = rt.block_on(validate_cmd.execute()).unwrap();
            black_box(report);
        });
    });
}

criterion_group!(
    benches,
    bench_validate_small,
    bench_validate_medium,
    bench_validate_large
);
criterion_main!(benches);
