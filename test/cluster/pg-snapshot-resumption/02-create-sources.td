# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

> DROP SOURCE IF EXISTS mz_source CASCADE;
> DROP SECRET IF EXISTS pgpass CASCADE;
> DROP CONNECTION IF EXISTS pg CASCADE;

> CREATE CLUSTER storage REPLICAS (
    r1 (
      STORAGECTL ADDRESS 'storage:2100',
      COMPUTECTL ADDRESSES ['storage:2101'],
      COMPUTE ADDRESSES ['storage:2102']
    )
  )

> CREATE SECRET pgpass AS 'postgres'
> CREATE CONNECTION pg TO POSTGRES (
    HOST postgres,
    DATABASE postgres,
    USER postgres,
    PASSWORD SECRET pgpass
  )

> CREATE SOURCE mz_source
  IN CLUSTER storage
  FROM POSTGRES
  CONNECTION pg
  (PUBLICATION 'mz_source')
  FOR ALL TABLES;
