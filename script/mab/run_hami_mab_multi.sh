#!/bin/bash

# Launch a container and run multiple NPU test processes inside it.
# Usage: ./run_hami_mab_multi.sh <ID> <NUM_PROCS> [PRIORITY] [MODEL] [BATCH] [DURATION]
#   ID         : Instance identifier (used for container name and logs)
#   NUM_PROCS  : Number of Python worker processes to start inside the container
#   PRIORITY   : Container-wide priority (read by manager). Default 50.
#   MODEL      : Optional model name (default: resnet50)
#   BATCH      : Optional batch size (default: 32)
#   DURATION   : Optional duration seconds (default: 120)

if [ "$#" -lt 2 ]; then
    echo "Usage: $0 <ID> <NUM_PROCS> [PRIORITY] [MODEL] [BATCH] [DURATION]"
    exit 1
fi

ID=$1
NUM_PROCS=$2
PRIORITY=${3:-50}
MODEL=${4:-resnet50}
BATCH=${5:-32}
DURATION=${6:-120}

CONTAINER_NAME="mab_hami${ID}"
LOG_PREFIX="inst${ID}"

# Path Configuration
IMAGE="registry-cbu.huawei.com/ascend/vllm-ascend:v0.10.1rc1"
HOST_PROJECT_DIR="/mnt/nvme0/mab"
CONTAINER_PROJECT_DIR="/mab"
HOST_SHARED_DIR="/tmp/hami-shared-region"
GLOBAL_SHM_PATH="/hami-shared-region/global_registry"
LIMITER_PATH="/mab/hami-vnpu-core/target/debug/limiter"
LIBRARY_PATH="/mab/hami-vnpu-core/target/debug/libvnpu.so"

# ==========================================
# 1. Environment Setup
# ==========================================
if [ ! -d "$HOST_PROJECT_DIR" ]; then
    sudo mkdir -p "$HOST_PROJECT_DIR"
    sudo chmod 777 "$HOST_PROJECT_DIR"
fi

if [ ! -d "$HOST_SHARED_DIR" ]; then
    sudo mkdir -p "$HOST_SHARED_DIR"
    sudo chmod 777 "$HOST_SHARED_DIR"
fi

# ==========================================
# 2. Launch Container with global preload
# ==========================================
echo "--- [Host] Launching Container: $CONTAINER_NAME ---"

docker run --rm -it --privileged -u root \
    --network=host --pid=host \
    --device=/dev/davinci0 --device=/dev/davinci1 --device=/dev/davinci2 --device=/dev/davinci3 \
    --device=/dev/davinci4 --device=/dev/davinci5 --device=/dev/davinci6 --device=/dev/davinci7 \
    --device=/dev/davinci_manager --device=/dev/devmm_svm --device=/dev/hisi_hdc \
    -v $HOST_SHARED_DIR:/hami-shared-region \
    -v /usr/local/bin/npu-smi:/usr/local/bin/npu-smi \
    -v /etc/ascend_install.info:/etc/ascend_install.info \
    -v /usr/local/Ascend/driver/lib64/driver:/usr/local/Ascend/driver/lib64/driver \
    -v /usr/local/Ascend/driver/version.info:/usr/local/Ascend/driver/version.info \
    -v $HOST_PROJECT_DIR:$CONTAINER_PROJECT_DIR \
    -v /mnt/nvme0/LLMs/:/models \
    --name "$CONTAINER_NAME" \
    -e LD_PRELOAD=$LIBRARY_PATH \
    -e NPU_GLOBAL_SHM_PATH=$GLOBAL_SHM_PATH \
    -e NPU_LOCAL_SHM_NAME=vnpu_local_session_${ID} \
    -e NPU_PRIORITY=$PRIORITY \
    -e ASCEND_RT_VISIBLE_DEVICES=1 \
    -e NPU_TOKEN_SCALE=200.0 \
    -e HOME=/ \
    $IMAGE \
    bash -lc "
        cd $CONTAINER_PROJECT_DIR
        # 1. Start Manager
        ${LIMITER_PATH} > ${CONTAINER_PROJECT_DIR}/${LOG_PREFIX}_manager.log 2>&1 &

        sleep 2

        # 2. Start multi-process Python test (all stdout visible here, also logged)
        echo '[Container] Starting multi-process test...'
        export LD_PRELOAD=${LIBRARY_PATH}
        python3 -u ${CONTAINER_PROJECT_DIR}/hami-vnpu-core/script/mab/multi_process_test.py \
            --num-procs ${NUM_PROCS} \
            --model ${MODEL} \
            --batch-size ${BATCH} \
            --duration ${DURATION} \
            --prio ${PRIORITY} \
            --log-prefix ${LOG_PREFIX} \
            | tee ${CONTAINER_PROJECT_DIR}/${LOG_PREFIX}_apps.log
    "
