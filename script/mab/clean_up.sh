#!/bin/bash

# 1. Configuration (Matching your run script)
HOST_PROJECT_DIR="/mnt/nvme0/mab"
HOST_SHARED_DIR="/tmp/hami-shared-region"

echo "--- [Host] Cleaning up MAB Environment ---"

# 2. Kill any lingering containers
echo "Stopping all MAB containers..."
docker ps -a | grep "mab_hami" | awk '{print $1}' | xargs -r docker stop
docker ps -a | grep "mab_hami" | awk '{print $1}' | xargs -r docker rm

# 3. Remove the Shared Memory Registry (The "Nuclear" Option)
# This is crucial if the Manager crashed and locked the NPUs.
if [ -f "${HOST_SHARED_DIR}/global_registry" ]; then
    echo "Removing global_registry lock file..."
    sudo rm -f "${HOST_SHARED_DIR}/global_registry"
fi

# 4. Clear Old Logs (Optional)
echo "Deleting old instance logs..."
sudo rm -f ${HOST_PROJECT_DIR}/inst*_manager.log
sudo rm -f ${HOST_PROJECT_DIR}/inst*_app.log

echo "--- Cleanup Complete. Environment is fresh. ---"