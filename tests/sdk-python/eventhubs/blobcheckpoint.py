"""A Blob-backed checkpoint store for the azure-eventhub SDK, written on top of azure-storage-blob.

`azure-eventhub-checkpointstoreblob` is not installed in this environment, so this implements the SDK's `CheckpointStore`
interface the way Microsoft's package does (layout and metadata names as in `BlobCheckpointStore` of that package):

    ownership  blob  <fqn>/<eventhub>/<consumer group>/ownership/<partition id>   (all lower case)
                     metadata: ownerid                       blob etag = the ownership etag, last-modified = claim time
    checkpoint blob  <fqn>/<eventhub>/<consumer group>/checkpoint/<partition id>
                     metadata: offset, sequencenumber        (both stored as strings)

Claiming a partition is an optimistic upload: `If-None-Match: *` (create only) when the caller has no etag, `If-Match: <etag>`
otherwise; a lost race (412 / 409) means "someone else claimed it" and that partition is left out of the result.
"""

from typing import Any, Dict, Iterable, Optional, Union

from azure.core import MatchConditions
from azure.core.exceptions import ResourceExistsError, ResourceModifiedError, ResourceNotFoundError
from azure.eventhub import CheckpointStore
from azure.storage.blob import ContainerClient


def _prefix(fqn: str, eventhub: str, group: str, kind: str) -> str:
    return f"{fqn}/{eventhub}/{group}/{kind}".lower() + "/"


class BlobCheckpointStore(CheckpointStore):
    def __init__(self, container_client: ContainerClient):
        self._container = container_client

    @classmethod
    def from_connection_string(cls, conn_str: str, container_name: str, **kwargs: Any) -> "BlobCheckpointStore":
        return cls(ContainerClient.from_connection_string(conn_str, container_name, **kwargs))

    # -- ownership ----------------------------------------------------------------------------------------------------------
    def list_ownership(self, fully_qualified_namespace: str, eventhub_name: str, consumer_group: str, **kwargs: Any) -> Iterable[Dict[str, Any]]:
        prefix = _prefix(fully_qualified_namespace, eventhub_name, consumer_group, "ownership")
        out = []
        for blob in self._container.list_blobs(name_starts_with=prefix, include=["metadata"], **kwargs):
            meta = blob.metadata or {}
            out.append({
                "fully_qualified_namespace": fully_qualified_namespace,
                "eventhub_name": eventhub_name,
                "consumer_group": consumer_group,
                "partition_id": blob.name.split("/")[-1],
                "owner_id": meta.get("ownerid") or "",  # a released claim stores an empty value, which the SDK reads back as None
                "last_modified_time": blob.last_modified.timestamp(),
                "etag": blob.etag,
            })
        return out

    def _claim_one(self, ownership: Dict[str, Any], **kwargs: Any) -> Optional[Dict[str, Any]]:
        name = _prefix(ownership["fully_qualified_namespace"], ownership["eventhub_name"], ownership["consumer_group"], "ownership") + str(
            ownership["partition_id"]
        )
        etag = ownership.get("etag")
        condition = {"etag": etag, "match_condition": MatchConditions.IfNotModified} if etag else {"match_condition": MatchConditions.IfMissing}
        try:
            props = self._container.get_blob_client(name).upload_blob(
                data=b"", overwrite=True, metadata={"ownerid": ownership["owner_id"]}, **condition, **kwargs,
            )
        except (ResourceModifiedError, ResourceExistsError, ResourceNotFoundError):
            return None  # another processor claimed it first (or the blob vanished): not ours
        claimed = dict(ownership)
        claimed["etag"] = props["etag"]
        claimed["last_modified_time"] = props["last_modified"].timestamp()
        return claimed

    def claim_ownership(self, ownership_list: Iterable[Dict[str, Any]], **kwargs: Any) -> Iterable[Dict[str, Any]]:
        result = []
        for ownership in ownership_list:
            claimed = self._claim_one(ownership, **kwargs)
            if claimed is not None:
                result.append(claimed)
        return result

    # -- checkpoints -----------------------------------------------------------------------------------------------------------
    def update_checkpoint(self, checkpoint: Dict[str, Optional[Union[str, int]]], **kwargs: Any) -> None:
        name = _prefix(checkpoint["fully_qualified_namespace"], checkpoint["eventhub_name"], checkpoint["consumer_group"], "checkpoint") + str(
            checkpoint["partition_id"]
        )
        metadata = {"offset": str(checkpoint["offset"]), "sequencenumber": str(checkpoint["sequence_number"])}
        self._container.get_blob_client(name).upload_blob(data=b"", overwrite=True, metadata=metadata, **kwargs)

    def list_checkpoints(self, fully_qualified_namespace: str, eventhub_name: str, consumer_group: str, **kwargs: Any) -> Iterable[Dict[str, Any]]:
        prefix = _prefix(fully_qualified_namespace, eventhub_name, consumer_group, "checkpoint")
        out = []
        for blob in self._container.list_blobs(name_starts_with=prefix, include=["metadata"], **kwargs):
            meta = blob.metadata or {}
            out.append({
                "fully_qualified_namespace": fully_qualified_namespace,
                "eventhub_name": eventhub_name,
                "consumer_group": consumer_group,
                "partition_id": blob.name.split("/")[-1],
                "offset": meta["offset"],
                "sequence_number": int(meta["sequencenumber"]),
            })
        return out
