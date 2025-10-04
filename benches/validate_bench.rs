use criterion::{black_box, criterion_group, criterion_main, Criterion};
use plexify::commands::validate::ValidateCommand;
use plexify::ignore::IgnoreFilter;
use std::fs;
use tempfile::TempDir;
use tokio::runtime::Runtime;
use walkdir::WalkDir;

fn create_test_media_library_with_ignored_dirs(size: usize) -> TempDir {
    let temp_dir = TempDir::new().unwrap();
    let media_root = temp_dir.path();

    // Create .plexifyignore file that ignores large directories
    fs::write(
        media_root.join(".plexifyignore"),
        "Downloads/\nTemp/\n*.tmp\ntools/",
    )
    .unwrap();

    // Create large ignored directories with many files
    for dir_name in ["Downloads", "Temp", "tools"] {
        fs::create_dir_all(media_root.join(dir_name)).unwrap();
        for i in 0..size {
            fs::write(
                media_root.join(format!("{}/file_{}.mkv", dir_name, i)),
                "content",
            )
            .unwrap();
        }
    }

    // Create valid media directories with fewer files
    for i in 0..(size / 10) {
        let show_path = media_root.join(format!("Series/Test Show {}/Season 01", i % 5));
        fs::create_dir_all(&show_path).unwrap();
        fs::write(
            show_path.join(format!(
                "Test Show {} - s01e{:02} - Episode {}.mkv",
                i % 5,
                (i % 12) + 1,
                i
            )),
            "",
        )
        .unwrap();
    }

    temp_dir
}

fn bench_directory_skip_old_approach(c: &mut Criterion) {
    c.bench_function("directory_skip_old_approach", |b| {
        b.iter(|| {
            let temp_dir = create_test_media_library_with_ignored_dirs(200);
            let root = temp_dir.path();
            let filter = IgnoreFilter::new(root.to_path_buf()).unwrap();

            // Old approach: visit all files but ignore them individually
            let mut count = 0;
            for entry in WalkDir::new(&root).follow_links(false).into_iter() {
                if let Ok(entry) = entry {
                    let path = entry.path();
                    if !path.is_dir() {
                        // Only check files, not directories
                        if !filter.should_ignore(path) {
                            count += 1;
                        }
                    }
                }
            }
            black_box(count);
        });
    });
}

fn bench_directory_skip_new_approach(c: &mut Criterion) {
    c.bench_function("directory_skip_new_approach", |b| {
        b.iter(|| {
            let temp_dir = create_test_media_library_with_ignored_dirs(200);
            let root = temp_dir.path();
            let filter = IgnoreFilter::new(root.to_path_buf()).unwrap();

            // New approach: skip entire directories before traversing
            let mut count = 0;
            for entry in WalkDir::new(&root)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| {
                    let path = e.path();
                    if path == root {
                        return true;
                    }
                    if path.is_dir() && filter.should_skip_dir(path) {
                        return false; // Skip entire directory
                    }
                    true
                })
            {
                if let Ok(entry) = entry {
                    let path = entry.path();
                    if !path.is_dir() {
                        // Only count files
                        if !filter.should_ignore(path) {
                            count += 1;
                        }
                    }
                }
            }
            black_box(count);
        });
    });
}

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
            let validate_cmd = ValidateCommand::new(temp_dir.path().to_path_buf(), false);
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
            let validate_cmd = ValidateCommand::new(temp_dir.path().to_path_buf(), false);
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
            let validate_cmd = ValidateCommand::new(temp_dir.path().to_path_buf(), false);
            let report = rt.block_on(validate_cmd.execute()).unwrap();
            black_box(report);
        });
    });
}

criterion_group!(
    benches,
    bench_validate_small,
    bench_validate_medium,
    bench_validate_large,
    bench_directory_skip_old_approach,
    bench_directory_skip_new_approach
);
criterion_main!(benches);
