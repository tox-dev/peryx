use std::hint::black_box;

use criterion::{BenchmarkId, Criterion};
use peryx_identity::{GrantScope, Role, UserId};
use peryx_storage::meta::{MetaStore, RoleGrantFilter, RoleGrantQuery};

const USER_COUNT: usize = 1_024;

fn main() {
    let (_dir, store, first, last) = dataset();
    let mut criterion = Criterion::default().configure_from_args();
    {
        let mut group = criterion.benchmark_group("managed_grant_user_page");
        for (position, user) in [("first", first), ("last", last)] {
            let query = RoleGrantQuery {
                filter: RoleGrantFilter::User(user),
                cursor: None,
                limit: 1,
            };
            group.bench_with_input(BenchmarkId::new("prefix", position), &query, |bencher, query| {
                bencher.iter(|| black_box(store.list_managed_grants(black_box(query)).unwrap()));
            });
        }
        group.finish();
    }
    criterion.final_summary();
}

fn dataset() -> (tempfile::TempDir, MetaStore, UserId, UserId) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let mut users = (0..USER_COUNT)
        .map(|position| store.create_user(&format!("user-{position}")).unwrap().id)
        .collect::<Vec<_>>();
    users.sort();
    for user in &users {
        store.grant_role(user, Role::Operator, GrantScope::Server).unwrap();
    }
    (dir, store, users[0].clone(), users[USER_COUNT - 1].clone())
}
