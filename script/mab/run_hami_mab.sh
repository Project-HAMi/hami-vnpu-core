#!/bin/bash

# 1. Validation
if [ "$#" -lt 2 ]; then
    echo "Usage: $0 <ID> <PRIORITY>"
    echo "Example: $0 1 20"
    exit 1
fi

ID=$1
PRIORITY=$2
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
# 2. Environment Setup
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
# 3. Launch Container
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
    -e NPU_GLOBAL_SHM_PATH=$GLOBAL_SHM_PATH \
    -e NPU_LOCAL_SHM_NAME=vnpu_local_session_${ID} \
    -e NPU_PRIORITY=$PRIORITY \
    -e ASCEND_RT_VISIBLE_DEVICES=1 \
    -e NPU_TOKEN_SCALE=200.0 \
    -e HOME=/ \
    $IMAGE \
    bash -c "
        cd $CONTAINER_PROJECT_DIR
        # Enable limiter debug logs (worker debug! included)
        # export RUST_LOG=debug
        # export NPU_FIXED_SHARE_RATIO=1
        # 1. Start Manager
        ${LIMITER_PATH} > ${CONTAINER_PROJECT_DIR}/${LOG_PREFIX}_manager.log 2>&1 &
        
        sleep 2
        
        # 2. Start AI App (Corrected Path below)
        echo '[Container] Starting Interactive Test...'
        LD_PRELOAD=${LIBRARY_PATH} python3 -u ${CONTAINER_PROJECT_DIR}/hami-vnpu-core/script/mab/interactive_test.py --prio=${PRIORITY}
    "