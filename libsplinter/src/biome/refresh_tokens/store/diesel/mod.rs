// Copyright 2018-2022 Cargill Incorporated
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod models;
mod operations;
mod schema;

use std::sync::{Arc, RwLock};

use diesel::r2d2::{ConnectionManager, Pool};

use crate::biome::refresh_tokens::store::{RefreshTokenError, RefreshTokenStore};
use crate::store::pool::ConnectionPool;

use operations::{
    add_token::RefreshTokenStoreAddTokenOperation,
    fetch_token::RefreshTokenStoreFetchTokenOperation,
    remove_token::RefreshTokenStoreRemoveTokenOperation,
    update_token::RefreshTokenStoreUpdateTokenOperation, RefreshTokenStoreOperations,
};

pub struct DieselRefreshTokenStore<C: diesel::Connection + 'static> {
    connection_pool: ConnectionPool<C>,
}

impl<C: diesel::Connection> DieselRefreshTokenStore<C> {
    pub fn new(connection_pool: Pool<ConnectionManager<C>>) -> Self {
        Self {
            connection_pool: connection_pool.into(),
        }
    }

    /// Create a new `DieselRefreshTokenStore` with write exclusivity enabled.
    ///
    /// Write exclusivity is enforced by providing a connection pool that is wrapped in a
    /// [`RwLock`]. This ensures that there may be only one writer, but many readers.
    ///
    /// # Arguments
    ///
    ///  * `connection_pool`: read-write lock-guarded connection pool for the database
    pub fn new_with_write_exclusivity(
        connection_pool: Arc<RwLock<Pool<ConnectionManager<C>>>>,
    ) -> Self {
        Self {
            connection_pool: connection_pool.into(),
        }
    }
}

#[cfg(feature = "postgres")]
impl RefreshTokenStore for DieselRefreshTokenStore<diesel::pg::PgConnection> {
    fn add_token(&self, user_id: &str, token: &str) -> Result<(), RefreshTokenError> {
        self.connection_pool
            .execute_write(|conn| RefreshTokenStoreOperations::new(conn).add_token(user_id, token))
    }
    fn remove_token(&self, user_id: &str) -> Result<(), RefreshTokenError> {
        self.connection_pool
            .execute_write(|conn| RefreshTokenStoreOperations::new(conn).remove_token(user_id))
    }
    fn update_token(&self, user_id: &str, token: &str) -> Result<(), RefreshTokenError> {
        self.connection_pool.execute_write(|conn| {
            RefreshTokenStoreOperations::new(conn).update_token(user_id, token)
        })
    }
    fn fetch_token(&self, user_id: &str) -> Result<String, RefreshTokenError> {
        self.connection_pool
            .execute_read(|conn| RefreshTokenStoreOperations::new(conn).fetch_token(user_id))
    }
}

#[cfg(feature = "sqlite")]
impl RefreshTokenStore for DieselRefreshTokenStore<diesel::sqlite::SqliteConnection> {
    fn add_token(&self, user_id: &str, token: &str) -> Result<(), RefreshTokenError> {
        self.connection_pool
            .execute_write(|conn| RefreshTokenStoreOperations::new(conn).add_token(user_id, token))
    }
    fn remove_token(&self, user_id: &str) -> Result<(), RefreshTokenError> {
        self.connection_pool
            .execute_write(|conn| RefreshTokenStoreOperations::new(conn).remove_token(user_id))
    }
    fn update_token(&self, user_id: &str, token: &str) -> Result<(), RefreshTokenError> {
        self.connection_pool.execute_write(|conn| {
            RefreshTokenStoreOperations::new(conn).update_token(user_id, token)
        })
    }
    fn fetch_token(&self, user_id: &str) -> Result<String, RefreshTokenError> {
        self.connection_pool
            .execute_read(|conn| RefreshTokenStoreOperations::new(conn).fetch_token(user_id))
    }
}

#[cfg(all(test, feature = "sqlite"))]
pub mod tests {
    use super::*;

    use crate::migrations::run_sqlite_migrations;

    use diesel::{
        r2d2::{ConnectionManager, Pool},
        sqlite::SqliteConnection,
    };

    /// Verify that a SQLite-backed `DieselRefreshTokenStore` correctly supports adding and
    /// fetching tokens.
    ///
    /// 1. Create a connection pool for an in-memory SQLite database and run migrations.
    /// 2. Create the `DieselRefreshTokenStore`.
    /// 3. Add some tokens.
    /// 4. Verify that the `fetch_token` method returns correct values for all existing tokens.
    /// 5. Verify that the `fetch_token` method returns a `RefreshTokenError::NotFoundError` for a
    ///    non-existent token.
    #[test]
    fn sqlite_add_and_fetch() {
        let pool = create_connection_pool_and_migrate();

        let store = DieselRefreshTokenStore::new(pool);

        store
            .add_token("user1", "token1")
            .expect("Failed to add token1");
        store
            .add_token("user2", "token2")
            .expect("Failed to add token2");
        store
            .add_token("user3", "token3")
            .expect("Failed to add token3");

        assert_eq!(
            store.fetch_token("user1").expect("Failed to fetch token1"),
            "token1",
        );
        assert_eq!(
            store.fetch_token("user2").expect("Failed to fetch token2"),
            "token2",
        );
        assert_eq!(
            store.fetch_token("user3").expect("Failed to fetch token3"),
            "token3",
        );

        match store.fetch_token("user4") {
            Err(RefreshTokenError::NotFoundError(_)) => {}
            res => panic!(
                "Expected Err(UserStoreError::NotFoundError), got {:?} instead",
                res
            ),
        }
    }

    /// Verify that a SQLite-backed `DieselRefreshTokenStore` correctly supports updating tokens.
    ///
    /// 1. Create a connection pool for an in-memory SQLite database and run migrations.
    /// 2. Create the `DieselRefreshTokenStore`.
    /// 3. Add a token and verify its existence in the store.
    /// 4. Update the token and verify that it is updated for the user.
    #[test]
    fn sqlite_update() {
        let pool = create_connection_pool_and_migrate();

        let store = DieselRefreshTokenStore::new(pool);

        store
            .add_token("user", "token1")
            .expect("Failed to add token");
        assert_eq!(
            store.fetch_token("user").expect("Failed to fetch token1"),
            "token1",
        );

        store
            .update_token("user", "token2")
            .expect("Failed to update token");
        assert_eq!(
            store.fetch_token("user").expect("Failed to fetch token2"),
            "token2",
        );
    }

    /// Verify that a SQLite-backed `DieselRefreshTokenStore` correctly supports removing tokens.
    ///
    /// 1. Create a connection pool for an in-memory SQLite database and run migrations.
    /// 2. Create the `DieselRefreshTokenStore`.
    /// 3. Add some tokens.
    /// 4. Remove a token and verify that the token no longer appears in the store.
    #[test]
    fn sqlite_remove() {
        let pool = create_connection_pool_and_migrate();

        let store = DieselRefreshTokenStore::new(pool);

        store
            .add_token("user1", "token1")
            .expect("Failed to add token1");
        store
            .add_token("user2", "token2")
            .expect("Failed to add token2");
        store
            .add_token("user3", "token3")
            .expect("Failed to add token3");

        store
            .remove_token("user3")
            .expect("Failed to remove token3");
        match store.fetch_token("user3") {
            Err(RefreshTokenError::NotFoundError(_)) => {}
            res => panic!(
                "Expected Err(RefreshTokenError::NotFoundError), got {:?} instead",
                res
            ),
        }
    }

    /// Creates a conneciton pool for an in-memory SQLite database with only a single connection
    /// available. Each connection is backed by a different in-memory SQLite database, so limiting
    /// the pool to a single connection insures that the same DB is used for all operations.
    fn create_connection_pool_and_migrate() -> Pool<ConnectionManager<SqliteConnection>> {
        let connection_manager = ConnectionManager::<SqliteConnection>::new(":memory:");
        let pool = Pool::builder()
            .max_size(1)
            .build(connection_manager)
            .expect("Failed to build connection pool");

        run_sqlite_migrations(&*pool.get().expect("Failed to get connection for migrations"))
            .expect("Failed to run migrations");

        pool
    }
}
