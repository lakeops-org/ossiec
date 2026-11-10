"""Run SQL on Spark for the engine test tier (ossiec writes Databricks' SQL as Spark's).

One process serves a whole test binary, so the JVM starts once. It reads one JSON
request per line, {"sql": "<statements separated by ;\\n>"}, runs each statement
and answers with one line: the marker, then {"rows": [[...], ...]} for the last
statement, or {"error": "..."}. Values are text, NULL is JSON null, so NULL stays
distinct from ''. Anything else Spark prints lacks the marker and is ignored.

Started by tests/common/engines.rs:
    docker exec -i ossiec-spark python3 /ossiec/spark_sql_server.py
"""

import datetime
import decimal
import glob
import json
import os
import sys

spark_home = os.environ.get("SPARK_HOME", "/opt/spark")
sys.path[:0] = [f"{spark_home}/python", *glob.glob(f"{spark_home}/python/lib/py4j-*.zip")]

from pyspark.sql import SparkSession  # noqa: E402

MARK = "\x1eOSSIEC "

spark = (
    SparkSession.builder.master("local[2]")
    .appName("ossiec")
    .config("spark.ui.enabled", "false")
    .config("spark.sql.shuffle.partitions", "2")
    .config("spark.sql.session.timeZone", "UTC")
    # A fresh warehouse per process: the catalog is in memory, and a table's
    # directory left by an earlier process would block creating it again.
    .config("spark.sql.warehouse.dir", f"/tmp/ossiec-warehouse-{os.getpid()}")
    .getOrCreate()
)
spark.sparkContext.setLogLevel("ERROR")


def text(v):
    if v is None:
        return None
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, decimal.Decimal):
        return format(v, "f")
    if isinstance(v, float):
        return repr(v)
    if isinstance(v, datetime.datetime):
        return v.isoformat(sep=" ")
    return str(v)


def answer(obj):
    sys.stdout.write(MARK + json.dumps(obj) + "\n")
    sys.stdout.flush()


answer({"ready": True})
for line in sys.stdin:
    if not line.strip():
        continue
    try:
        sql = json.loads(line)["sql"]
        rows = []
        for stmt in sql.split(";\n"):
            stmt = stmt.strip().rstrip(";").strip()
            if stmt:
                rows = spark.sql(stmt).collect()
        answer({"rows": [[text(v) for v in r] for r in rows]})
    except Exception as e:  # noqa: BLE001 - every failure is the query's answer
        answer({"error": " ".join(str(e).split())[:2000]})
