#!/usr/bin/env bash
set -euo pipefail
lt=$(jq -r .template <<<"$SPEC")
version=$(jq -r .version <<<"$SPEC")
labels=$(jq -r .labels <<<"$SPEC")
userdata="freemkv/$(jq -r .user_data <<<"$SPEC")"
bdm=$(jq -c .block_device_mappings <<<"$SPEC")
tags=$(jq -cn --arg p "$PARAM" --arg r "${REPO%%/*}" --arg id "$RUN_ID" --arg l "$labels" '[{ResourceType: "instance",
  Tags: [{Key: "freemkv-ci", Value: "runner"}, {Key: "runner-token-param", Value: $p},
         {Key: "runner-repo", Value: $r}, {Key: "launched-by", Value: $id},
         {Key: "runner-labels", Value: $l}]}]')
vpc=$(aws ec2 describe-vpcs --filters Name=isDefault,Values=true --query 'Vpcs[0].VpcId' --output text)
subnets=$(aws ec2 describe-subnets --filters "Name=vpc-id,Values=$vpc" --query 'Subnets[].SubnetId' --output text)
[ -n "$subnets" ] || { echo "no subnets in default VPC $vpc" >&2; exit 1; }
# The functional Linux leg falls back across types (the actual type is recorded
# by the leg); a perf leg has exactly one type; an empty list is the template's.
types=$(jq -r '.types | if length == 0 then "template" else join(" ") end' <<<"$SPEC")
# On-Demand runs WITHOUT the template (run-instances cannot clear a template's Spot
# market options): launch-spec copies the pinned version's image, instance profile,
# security groups, volumes, metadata options and shutdown behaviour into $od, plus
# its own instance tags (Name) next to ours. aws ec2 run-instances help, --cli-input-json:
# "If other arguments are provided on the command line, those values will override
# the JSON-provided values." (so --instance-type picks the attempt's type).
od=$(jq -c '.on_demand.input // empty' <<<"$SPEC")
od_tags=$(jq -c --argjson t "$tags" '[{ResourceType: "instance", Tags: ($t[0].Tags + (.on_demand.tags // []))}]' <<<"$SPEC")
try_launch() {
  local market="$1" options="$2" via_template="$3"
  local t s id
  for t in $types; do
    for s in $subnets; do
      echo "attempt: type=$t subnet=$s market=$market template=$via_template"
      extra=()
      [ "$t" = template ] || extra+=(--instance-type "$t")
      if [ "$via_template" = true ]; then
        extra+=(--launch-template "LaunchTemplateName=$lt,Version=$version" --tag-specifications "$tags")
        [ "$options" = null ] || extra+=(--instance-market-options "$options")
        [ "$bdm" = null ] || extra+=(--block-device-mappings "$bdm")
      else
        [ -n "$od" ] || { echo "no On-Demand input in the spec" > launch.err; return 2; }
        extra+=(--cli-input-json "$od" --tag-specifications "$od_tags")
      fi
      if id=$(aws ec2 run-instances \
                --subnet-id "$s" "${extra[@]}" \
                --user-data "file://$userdata" \
                --query 'Instances[0].InstanceId' --output text 2>launch.err); then
        echo "instance=$id" >> "$GITHUB_OUTPUT"
        echo "launched $id ($LEG, type=$t subnet=$s market=$market, template $lt v$version)"
        # What record-media-evidence checks the leg against, even after EC2 forgets the instance.
        jq -n --arg id "$id" --arg run "$RUN_ID" --arg attempt "$ATTEMPT" --arg leg "$LEG" \
              --arg os "$(jq -r .os <<<"$SPEC")" --arg labels "$labels" \
              --arg lt "$lt" --arg v "$version" --arg type "$t" --arg market "$market" \
              '{instance_id: $id, launched_by: ($run | tonumber), run_attempt: ($attempt | tonumber),
                leg: $leg, os: $os, runner_labels: $labels, market: $market,
                launch_template: $lt, launch_template_version: $v, requested_type: $type}' \
              > "launch-$LEG-$ATTEMPT.json"
        return 0
      fi
      echo "  failed ($t/$s/$market): $(cat launch.err)"
      if ! grep -qiE 'InsufficientInstanceCapacity|Unsupported|InsufficientHostCapacity|capacity|SpotMaxPriceTooLow' launch.err; then
        return 2
      fi
    done
  done
  return 1
}
# Markets in order (Spot at the policy's cap through the template, then
# On-Demand without it; or the template's own): only capacity exhaustion moves on.
rc=1
while read -r market; do
  name=$(jq -r .name <<<"$market")
  options=$(jq -c .options <<<"$market")
  via_template=$(jq -r .template <<<"$market")
  rc=0; try_launch "$name" "$options" "$via_template" || rc=$?
  [ "$rc" = 1 ] || break
  echo "every $name attempt hit capacity"
done < <(jq -c '.markets[]' <<<"$SPEC")
[ "$rc" = 0 ] && exit 0
echo "::error title=Runner launch failed::could not launch the $LEG media runner — cancelling the run so its leg does not sit queued" >&2
exit 1
